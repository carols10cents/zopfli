use std::cmp;
use std::io::{self, Read, Write};

use blocksplitter::{blocksplit, blocksplit_lz77};
use katajainen::length_limited_code_lengths;
use lz77::{ZopfliBlockState, Lz77Store, LitLen};
use squeeze::{lz77_optimal_fixed, lz77_optimal};
use symbols::{get_length_symbol, get_dist_symbol, get_length_symbol_extra_bits, get_dist_symbol_extra_bits, get_length_extra_bits_value, get_length_extra_bits, get_dist_extra_bits_value, get_dist_extra_bits};
use tree::{lengths_to_symbols};
use util::{ZOPFLI_NUM_LL, ZOPFLI_NUM_D, ZOPFLI_MASTER_BLOCK_SIZE, ZOPFLI_WINDOW_SIZE};
use Options;
use iter::IsFinalIterator;

/// Compresses according to the deflate specification and append the compressed
/// result to the output.
///
/// `options`: global program options
/// `btype`: the deflate block type. Use Dynamic for best compression.
///   - Uncompressed: non compressed blocks (00)
///   - Fixed: blocks with fixed tree (01)
///   - Dynamic: blocks with dynamic tree (10)
/// `in_data`: the input bytes
/// `insize`: the input size
/// `out`: pointer to the dynamic output array to which the result is appended. Must
///   be freed after use.
pub fn deflate<R, W>(options: &Options, btype: BlockType, mut in_data: R, insize: u64, out: W) -> io::Result<()>
    where R: Read, W: Write
{
    let mut block_and_window = [0; ZOPFLI_WINDOW_SIZE + ZOPFLI_MASTER_BLOCK_SIZE];
    let block_and_window_len = block_and_window.len();

    let mut bitwise_writer = BitwiseWriter::new(out);
    let mut total_bytes_read = 0;
    let mut bytes_read;
    let mut i = 0;
    let mut j = ZOPFLI_MASTER_BLOCK_SIZE;

    loop {
        bytes_read = try!(in_data.read(&mut block_and_window[i..j]));

        if bytes_read == 0 {
            // Reached EOF while trying to read a full block.
            // This means that insize is not a multiple of ZOPFLI_MASTER_BLOCK_SIZE, and the
            // last block should be smaller.
            break;
        }

        total_bytes_read += bytes_read as u64;

        if bytes_read == j - i {
            // Just read a full block.
            let final_block = total_bytes_read == insize;
            let instart = if j == ZOPFLI_MASTER_BLOCK_SIZE { 0 } else { ZOPFLI_WINDOW_SIZE };
            try!(deflate_part(options, btype, final_block, &mut block_and_window, instart, j, &mut bitwise_writer));

            if final_block {
                // No point in trying to read more data.
                break;
            } else {
                // Update buffer start and end indexes accordingly.
                // Keep a sliding window at the beginning with the latest ZOPFLI_WINDOW_SIZE bytes.
                // There is no purpose in keeping more than that because only the strings in the
                // previous ZOPFLI_WINDOW_SIZE bytes can be referenced in a DEFLATE stream.
                block_and_window.copy_within(j - ZOPFLI_WINDOW_SIZE..j, 0);
                i = ZOPFLI_WINDOW_SIZE;
                j = block_and_window_len;
            }
        } else {
            // Continue trying to read a full block.
            i += bytes_read;
        }
    }

    if bytes_read == 0 {
        let instart = if j == ZOPFLI_MASTER_BLOCK_SIZE { 0 } else { ZOPFLI_WINDOW_SIZE };
        let inend = instart + (total_bytes_read % ZOPFLI_MASTER_BLOCK_SIZE as u64) as usize;
        try!(deflate_part(options, btype, true, &mut block_and_window, instart, inend, &mut bitwise_writer));
    }

    bitwise_writer.finish_partial_bits()
}

/// Deflate a part, to allow deflate() to use multiple master blocks if
/// needed.
/// It is possible to call this function multiple times in a row, shifting
/// instart and inend to next bytes of the data. If instart is larger than 0, then
/// previous bytes are used as the initial dictionary for LZ77.
/// This function will usually output multiple deflate blocks. If final is true, then
/// the final bit will be set on the last block.
/// Like deflate, but allows to specify start and end byte with instart and
/// inend. Only that part is compressed, but earlier bytes are still used for the
/// back window.
fn deflate_part<W>(options: &Options, btype: BlockType, final_block: bool, in_data: &[u8], instart: usize, inend: usize, bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    /* If btype=Dynamic is specified, it tries all block types. If a lesser btype is
    given, then however it forces that one. Neither of the lesser types needs
    block splitting as they have no dynamic huffman trees. */
    match btype {
        BlockType::Uncompressed => {
            add_non_compressed_block(final_block, in_data, instart, inend, bitwise_writer)
        },
        BlockType::Fixed => {
            let mut store = Lz77Store::new();
            let mut s = ZopfliBlockState::new(options, instart, inend);

            lz77_optimal_fixed(&mut s, in_data, instart, inend, &mut store);
            add_lz77_block(options, btype, final_block, in_data, &store, 0, store.size(), 0, bitwise_writer)
        },
        BlockType::Dynamic => {
            blocksplit_attempt(options, final_block, in_data, instart, inend, bitwise_writer)
        },
    }
}

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum BlockType {
    Uncompressed,
    Fixed,
    Dynamic,
}

fn fixed_tree() -> (Vec<u32>, Vec<u32>) {
    let mut ll = Vec::with_capacity(ZOPFLI_NUM_LL);
    ll.resize(144, 8);
    ll.resize(256, 9);
    ll.resize(280, 7);
    ll.resize(288, 8);
    let d = vec![5; ZOPFLI_NUM_D];
    (ll, d)
}

/// Changes the population counts in a way that the consequent Huffman tree
/// compression, especially its rle-part, will be more likely to compress this data
/// more efficiently. length contains the size of the histogram.
fn optimize_huffman_for_rle(counts: &mut [usize]) {
    let mut length = counts.len();
    // 1) We don't want to touch the trailing zeros. We may break the
    // rules of the format by adding more data in the distance codes.
    loop {
        if length == 0 {
            return;
        }
        if counts[length - 1] != 0 {
            // Now counts[0..length - 1] does not have trailing zeros.
            break;
        }
        length -= 1;
    }

    // 2) Let's mark all population counts that already can be encoded
    // with an rle code.
    let mut good_for_rle = vec![false; length];

    // Let's not spoil any of the existing good rle codes.
    // Mark any seq of 0's that is longer than 5 as a good_for_rle.
    // Mark any seq of non-0's that is longer than 7 as a good_for_rle.
    let mut symbol = counts[0];
    let mut stride = 0;
    for (i, &count) in counts.iter().enumerate().take(length) {
        if count != symbol {
            if (symbol == 0 && stride >= 5) || (symbol != 0 && stride >= 7) {
                for k in 0..stride {
                    good_for_rle[i - k - 1] = true;
                }
            }
            stride = 1;
            symbol = count;
        } else {
            stride += 1;
        }
    }

    // 3) Let's replace those population counts that lead to more rle codes.
    stride = 0;
    let mut limit = counts[0];
    let mut sum = 0;
    for i in 0..(length + 1) {
        // Heuristic for selecting the stride ranges to collapse.
        if i == length || good_for_rle[i] || (counts[i] as i32 - limit as i32).abs() >= 4 {
            if stride >= 4 || (stride >= 3 && sum == 0) {
                // The stride must end, collapse what we have, if we have enough (4).
                let count = if sum == 0 {
                    // Don't upgrade an all zeros stride to ones.
                    0
                } else {
                    cmp::max((sum + stride / 2) / stride, 1)
                };
                set_counts_to_count(counts, count, i, stride);
            }
            stride = 0;
            sum = 0;
            if length > 2 && i < length - 3 {
                // All interesting strides have a count of at least 4,
                // at least when non-zeros.
                limit = (counts[i] + counts[i + 1] + counts[i + 2] + counts[i + 3] + 2) / 4;
            } else if i < length {
                limit = counts[i];
            } else {
                limit = 0;
            }
        }
        stride += 1;
        if i != length {
            sum += counts[i];
        }
    }
}

// Ensures there are at least 2 distance codes to support buggy decoders.
// Zlib 1.2.1 and below have a bug where it fails if there isn't at least 1
// distance code (with length > 0), even though it's valid according to the
// deflate spec to have 0 distance codes. On top of that, some mobile phones
// require at least two distance codes. To support these decoders too (but
// potentially at the cost of a few bytes), add dummy code lengths of 1.
// References to this bug can be found in the changelog of
// Zlib 1.2.2 and here: http://www.jonof.id.au/forum/index.php?topic=515.0.
//
// d_lengths: the 32 lengths of the distance codes.
fn patch_distance_codes_for_buggy_decoders(d_lengths: &mut[u32]) {
    // Ignore the two unused codes from the spec
    let num_dist_codes = d_lengths.iter().take(30).filter(|&&d_length| d_length != 0).count();

    match num_dist_codes {
        0 => {
            d_lengths[0] = 1;
            d_lengths[1] = 1;
        },
        1 => {
            let index = if d_lengths[0] == 0 {
                0
            } else {
                1
            };
            d_lengths[index] = 1;
        },
        _ => {}, // Two or more codes is fine.
    }
}

/// Same as `calculate_block_symbol_size`, but for block size smaller than histogram
/// size.
fn calculate_block_symbol_size_small(ll_lengths: &[u32], d_lengths: &[u32], lz77: &Lz77Store, lstart: usize, lend: usize) -> usize {
    let mut result = 0;

    debug_assert!(lend - 1 < lz77.size());

    for &item in &lz77.litlens[lstart..lend] {
        match item {
            LitLen::Literal(litlens_i) => {
                debug_assert!(litlens_i < 259);
                result += ll_lengths[litlens_i as usize]
            },
            LitLen::LengthDist(litlens_i, dists_i) => {
                debug_assert!(litlens_i < 259);
                let ll_symbol = get_length_symbol(litlens_i as usize);
                let d_symbol = get_dist_symbol(dists_i as i32);
                result += ll_lengths[ll_symbol as usize];
                result += d_lengths[d_symbol as usize];
                result += get_length_symbol_extra_bits(ll_symbol) as u32;
                result += get_dist_symbol_extra_bits(d_symbol) as u32;
            },
        }
    }
    result += ll_lengths[256]; // end symbol
    result as usize
}

/// Same as `calculate_block_symbol_size`, but with the histogram provided by the caller.
fn calculate_block_symbol_size_given_counts(ll_counts: &[usize], d_counts: &[usize], ll_lengths: &[u32], d_lengths: &[u32], lz77: &Lz77Store, lstart: usize, lend: usize) -> usize {
    if lstart + ZOPFLI_NUM_LL * 3 > lend {
        calculate_block_symbol_size_small(ll_lengths, d_lengths, lz77, lstart, lend)
    } else {
        let mut result = 0;
        for i in 0..256 {
            result += ll_lengths[i] * ll_counts[i] as u32;
        }
        for i in 257..286 {
            result += ll_lengths[i] * ll_counts[i] as u32;
            result += (get_length_symbol_extra_bits(i as i32) * ll_counts[i] as i32) as u32;
        }
        for i in 0..30 {
            result += d_lengths[i] * d_counts[i] as u32;
            result += (get_dist_symbol_extra_bits(i as i32) * d_counts[i] as i32) as u32;
        }
        result += ll_lengths[256]; // end symbol
        result as usize
    }
}

/// Calculates size of the part after the header and tree of an LZ77 block, in bits.
fn calculate_block_symbol_size(ll_lengths: &[u32], d_lengths: &[u32], lz77: &Lz77Store, lstart: usize, lend: usize) -> usize {
    if lstart + ZOPFLI_NUM_LL * 3 > lend {
        calculate_block_symbol_size_small(ll_lengths, d_lengths, lz77, lstart, lend)
    } else {
        let (ll_counts, d_counts) = lz77.get_histogram(lstart, lend);
        calculate_block_symbol_size_given_counts(&ll_counts, &d_counts, ll_lengths, d_lengths, lz77, lstart, lend)
    }
}

/// Encodes the Huffman tree and returns how many bits its encoding takes; only returns the size
/// and runs faster.
fn encode_tree_no_output(ll_lengths: &[u32], d_lengths: &[u32], use_16: bool, use_17: bool, use_18: bool) -> usize {
    let mut hlit = 29;  /* 286 - 257 */
    let mut hdist = 29;  /* 32 - 1, but gzip does not like hdist > 29.*/

    let mut clcounts = [0; 19];
    /* The order in which code length code lengths are encoded as per deflate. */
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15
    ];
    let mut result_size = 0;

    /* Trim zeros. */
    while hlit > 0 && ll_lengths[257 + hlit - 1] == 0 {
        hlit -= 1;
    }
    while hdist > 0 && d_lengths[1 + hdist - 1] == 0 {
        hdist -= 1;
    }
    let hlit2 = hlit + 257;

    let lld_total = hlit2 + hdist + 1; /* Total amount of literal, length, distance codes. */

    let mut i = 0;

    while i < lld_total {
        /* This is an encoding of a huffman tree, so now the length is a symbol */
        let symbol = if i < hlit2 {
            ll_lengths[i]
        } else {
            d_lengths[i - hlit2]
        } as u8;

        let mut count = 1;
        if use_16 || (symbol == 0 && (use_17 || use_18)) {
            let mut j = i + 1;
            let mut symbol_calc = if j < hlit2 {
                ll_lengths[j]
            } else {
                d_lengths[j - hlit2]
            } as u8;

            while j < lld_total && symbol == symbol_calc {
                count += 1;
                j += 1;
                symbol_calc = if j < hlit2 {
                    ll_lengths[j]
                } else {
                    d_lengths[j - hlit2]
                } as u8;
            }
        }

        i += count - 1;

        /* Repetitions of zeroes */
        if symbol == 0 && count >= 3 {
            if use_18 {
                while count >= 11 {
                    let count2 = if count > 138 {
                        138
                    } else {
                        count
                    };
                    clcounts[18] += 1;
                    count -= count2;
                }
            }
            if use_17 {
                while count >= 3 {
                    let count2 = if count > 10 {
                        10
                    } else {
                        count
                    };
                    clcounts[17] += 1;
                    count -= count2;
                }
            }
        }

        /* Repetitions of any symbol */
        if use_16 && count >= 4 {
            count -= 1;  /* Since the first one is hardcoded. */
            clcounts[symbol as usize] += 1;
            while count >= 3 {
                let count2 = if count > 6 {
                    6
                } else {
                    count
                };
                clcounts[16] += 1;
                count -= count2;
            }
        }

        /* No or insufficient repetition */
        clcounts[symbol as usize] += count;
        while count > 0 {
            count -= 1;
        }
        i += 1;
    }

    let clcl = length_limited_code_lengths(&clcounts, 7);

    let mut hclen = 15;
    /* Trim zeros. */
    while hclen > 0 && clcounts[order[hclen + 4 - 1]] == 0 {
        hclen -= 1;
    }

    result_size += 14;  /* hlit, hdist, hclen bits */
    result_size += (hclen + 4) * 3;  /* clcl bits */
    for i in 0..19 {
        result_size += clcl[i] as usize * clcounts[i];
    }
    /* Extra bits. */
    result_size += clcounts[16] * 2;
    result_size += clcounts[17] * 3;
    result_size += clcounts[18] * 7;

    result_size
}

static TRUTH_TABLE: [(bool, bool, bool); 8] = [
    (false, false, false),
    (true, false, false),
    (false, true, false),
    (true, true, false),
    (false, false, true),
    (true, false, true),
    (false, true, true),
    (true, true, true),
];

/// Gives the exact size of the tree, in bits, as it will be encoded in DEFLATE.
fn calculate_tree_size(ll_lengths: &[u32], d_lengths: &[u32]) -> usize {
    TRUTH_TABLE.iter().map(|&(use_16, use_17, use_18)| {
        encode_tree_no_output(ll_lengths, d_lengths, use_16, use_17, use_18)
    }).min().unwrap_or(0)
}

/// Encodes the Huffman tree and returns how many bits its encoding takes and returns output.
// TODO: This return value is unused.
fn encode_tree<W>(ll_lengths: &[u32], d_lengths: &[u32], use_16: bool, use_17: bool, use_18: bool, bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<usize>
    where W: Write
{
    let mut hlit = 29;  /* 286 - 257 */
    let mut hdist = 29;  /* 32 - 1, but gzip does not like hdist > 29.*/

    let mut clcounts = [0; 19];
    /* The order in which code length code lengths are encoded as per deflate. */
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15
    ];
    let mut result_size = 0;

    let mut rle = vec![];
    let mut rle_bits = vec![];

    /* Trim zeros. */
    while hlit > 0 && ll_lengths[257 + hlit - 1] == 0 {
        hlit -= 1;
    }
    while hdist > 0 && d_lengths[1 + hdist - 1] == 0 {
        hdist -= 1;
    }
    let hlit2 = hlit + 257;

    let lld_total = hlit2 + hdist + 1; /* Total amount of literal, length, distance codes. */

    let mut i = 0;

    while i < lld_total {
        /* This is an encoding of a huffman tree, so now the length is a symbol */
        let symbol = if i < hlit2 {
            ll_lengths[i]
        } else {
            d_lengths[i - hlit2]
        } as u8;

        let mut count = 1;
        if use_16 || (symbol == 0 && (use_17 || use_18 )) {
            let mut j = i + 1;
            let mut symbol_calc = if j < hlit2 {
                ll_lengths[j]
            } else {
                d_lengths[j - hlit2]
            } as u8;

            while j < lld_total && symbol == symbol_calc {
                count += 1;
                j += 1;
                symbol_calc = if j < hlit2 {
                    ll_lengths[j]
                } else {
                    d_lengths[j - hlit2]
                } as u8;
            }
        }

        i += count - 1;

        /* Repetitions of zeroes */
        if symbol == 0 && count >= 3 {
            if use_18 {
                while count >= 11 {
                    let count2 = if count > 138 {
                        138
                    } else {
                        count
                    };
                    rle.push(18);
                    rle_bits.push(count2 - 11);
                    clcounts[18] += 1;
                    count -= count2;
                }
            }
            if use_17 {
                while count >= 3 {
                    let count2 = if count > 10 {
                        10
                    } else {
                        count
                    };
                    rle.push(17);
                    rle_bits.push(count2 - 3);
                    clcounts[17] += 1;
                    count -= count2;
                }
            }
        }

        /* Repetitions of any symbol */
        if use_16 && count >= 4 {
            count -= 1;  /* Since the first one is hardcoded. */
            clcounts[symbol as usize] += 1;
            rle.push(symbol);
            rle_bits.push(0);

            while count >= 3 {
                let count2 = if count > 6 {
                    6
                } else {
                    count
                };
                rle.push(16);
                rle_bits.push(count2 - 3);
                clcounts[16] += 1;
                count -= count2;
            }
        }

        /* No or insufficient repetition */
        clcounts[symbol as usize] += count;
        while count > 0 {
            rle.push(symbol);
            rle_bits.push(0);
            count -= 1;
        }
        i += 1;
    }

    let clcl = length_limited_code_lengths(&clcounts, 7);
    let clsymbols = lengths_to_symbols(&clcl, 7);

    let mut hclen = 15;
    /* Trim zeros. */
    while hclen > 0 && clcounts[order[hclen + 4 - 1]] == 0 {
        hclen -= 1;
    }

    try!(bitwise_writer.add_bits(hlit as u32, 5));
    try!(bitwise_writer.add_bits(hdist as u32, 5));
    try!(bitwise_writer.add_bits(hclen as u32, 4));

    for &item in order.iter().take(hclen + 4) {
        try!(bitwise_writer.add_bits(clcl[item], 3));
    }

    for i in 0..rle.len() {
        let rle_i = rle[i] as usize;
        let rle_bits_i = rle_bits[i] as u32;
        let sym = clsymbols[rle_i];
        try!(bitwise_writer.add_huffman_bits(sym, clcl[rle_i]));
        /* Extra bits. */
        if rle_i == 16 {
            try!(bitwise_writer.add_bits(rle_bits_i, 2));
        } else if rle_i == 17 {
            try!(bitwise_writer.add_bits(rle_bits_i, 3));
        } else if rle_i == 18 {
            try!(bitwise_writer.add_bits(rle_bits_i, 7));
        }
    }

    result_size += 14;  /* hlit, hdist, hclen bits */
    result_size += (hclen + 4) * 3;  /* clcl bits */
    for i in 0..19 {
        result_size += clcl[i] as usize * clcounts[i];
    }
    /* Extra bits. */
    result_size += clcounts[16] * 2;
    result_size += clcounts[17] * 3;
    result_size += clcounts[18] * 7;

    Ok(result_size)
}

fn add_dynamic_tree<W>(ll_lengths: &[u32], d_lengths: &[u32], bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    let mut best = 0;
    let mut bestsize = 0;

    for i in 0..8 {
        let size = encode_tree_no_output(ll_lengths, d_lengths, i & 1 > 0, i & 2 > 0, i & 4 > 0);
        if bestsize == 0 || size < bestsize {
            bestsize = size;
            best = i;
        }
    }

    encode_tree(ll_lengths, d_lengths, best & 1 > 0, best & 2 > 0, best & 4 > 0, bitwise_writer).map(|_| ())
}

/// Adds a deflate block with the given LZ77 data to the output.
/// `options`: global program options
/// `btype`: the block type, must be `Fixed` or `Dynamic`
/// `final`: whether to set the "final" bit on this block, must be the last block
/// `litlens`: literal/length array of the LZ77 data, in the same format as in
///     `Lz77Store`.
/// `dists`: distance array of the LZ77 data, in the same format as in
///     `Lz77Store`.
/// `lstart`: where to start in the LZ77 data
/// `lend`: where to end in the LZ77 data (not inclusive)
/// `expected_data_size`: the uncompressed block size, used for assert, but you can
///   set it to `0` to not do the assertion.
/// `bitwise_writer`: writer responsible for appending bits
fn add_lz77_block<W>(options: &Options, btype: BlockType, final_block: bool, in_data: &[u8], lz77: &Lz77Store, lstart: usize, lend: usize, expected_data_size: usize, bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    if btype == BlockType::Uncompressed {
        let length = lz77.get_byte_range(lstart, lend);
        let pos = if lstart == lend {
            0
        } else {
            lz77.pos[lstart]
        };
        let end = pos + length;
        return add_non_compressed_block(final_block, in_data, pos, end, bitwise_writer);
    }

    try!(bitwise_writer.add_bit(final_block as u8));

    let (ll_lengths, d_lengths) = match btype {
        BlockType::Uncompressed => unreachable!(),
        BlockType::Fixed => {
            try!(bitwise_writer.add_bit(1));
            try!(bitwise_writer.add_bit(0));
            fixed_tree()
        },
        BlockType::Dynamic => {
            try!(bitwise_writer.add_bit(0));
            try!(bitwise_writer.add_bit(1));
            let (_, ll_lengths, d_lengths) = get_dynamic_lengths(lz77, lstart, lend);

            let detect_tree_size = bitwise_writer.bytes_written();
            try!(add_dynamic_tree(&ll_lengths, &d_lengths, bitwise_writer));
            if options.verbose {
                println!("treesize: {}", bitwise_writer.bytes_written() - detect_tree_size);
            }
            (ll_lengths, d_lengths)
        }
    };

    let ll_symbols = lengths_to_symbols(&ll_lengths, 15);
    let d_symbols = lengths_to_symbols(&d_lengths, 15);

    let detect_block_size = bitwise_writer.bytes_written();
    try!(add_lz77_data(lz77, lstart, lend, expected_data_size, &ll_symbols, &ll_lengths, &d_symbols, &d_lengths, bitwise_writer));

    /* End symbol. */
    try!(bitwise_writer.add_huffman_bits(ll_symbols[256], ll_lengths[256]));

    if options.verbose {
        let uncompressed_size = lz77.litlens[lstart..lend].iter().fold(0, |acc, &x| acc + x.size());
        let compressed_size = bitwise_writer.bytes_written() - detect_block_size;
        println!("compressed block size: {} ({}k) (unc: {})", compressed_size, compressed_size / 1024, uncompressed_size);
    }
    Ok(())
}

/// Calculates block size in bits.
/// litlens: lz77 lit/lengths
/// dists: ll77 distances
/// lstart: start of block
/// lend: end of block (not inclusive)
pub fn calculate_block_size(lz77: &Lz77Store, lstart: usize, lend: usize, btype: BlockType) -> f64 {
    match btype {
        BlockType::Uncompressed => {
            let length = lz77.get_byte_range(lstart, lend);
            let rem = length % 65535;
            let blocks = length / 65535 + (if rem > 0 { 1 } else { 0 });
            /* An uncompressed block must actually be split into multiple blocks if it's
               larger than 65535 bytes long. Eeach block header is 5 bytes: 3 bits,
               padding, LEN and NLEN (potential less padding for first one ignored). */
            (blocks * 5 * 8 + length * 8) as f64
        },
        BlockType::Fixed => {
            let fixed_tree = fixed_tree();
            let ll_lengths = fixed_tree.0;
            let d_lengths = fixed_tree.1;

            let mut result = 3.0; /* bfinal and btype bits */
            result += calculate_block_symbol_size(&ll_lengths, &d_lengths, lz77, lstart, lend) as f64;
            result
        },
        BlockType::Dynamic => {
            get_dynamic_lengths(lz77, lstart, lend).0 + 3.0
        },
    }
}

/// Tries out `OptimizeHuffmanForRle` for this block, if the result is smaller,
/// uses it, otherwise keeps the original. Returns size of encoded tree and data in
/// bits, not including the 3-bit block header.
fn try_optimize_huffman_for_rle(lz77: &Lz77Store, lstart: usize, lend: usize, ll_counts: &[usize], d_counts: &[usize], ll_lengths: Vec<u32>, d_lengths: Vec<u32>) -> (f64, Vec<u32>, Vec<u32>) {
    let mut ll_counts2 = ll_counts.to_owned();
    let mut d_counts2 = d_counts.to_owned();

    let treesize = calculate_tree_size(&ll_lengths, &d_lengths);
    let datasize = calculate_block_symbol_size_given_counts(ll_counts, d_counts, &ll_lengths, &d_lengths, lz77, lstart, lend);

    optimize_huffman_for_rle(&mut ll_counts2);
    optimize_huffman_for_rle(&mut d_counts2);

    let ll_lengths2 = length_limited_code_lengths(&ll_counts2, 15);
    let mut d_lengths2 = length_limited_code_lengths(&d_counts2, 15);
    patch_distance_codes_for_buggy_decoders(&mut d_lengths2[..]);

    let treesize2 = calculate_tree_size(&ll_lengths2, &d_lengths2);
    let datasize2 = calculate_block_symbol_size_given_counts(ll_counts, d_counts, &ll_lengths2, &d_lengths2, lz77, lstart, lend);

    if treesize2 + datasize2 < treesize + datasize {
        (((treesize2 + datasize2) as f64), ll_lengths2, d_lengths2)
    } else {
        ((treesize + datasize) as f64, ll_lengths, d_lengths)
    }
}

/// Calculates the bit lengths for the symbols for dynamic blocks. Chooses bit
/// lengths that give the smallest size of tree encoding + encoding of all the
/// symbols to have smallest output size. This are not necessarily the ideal Huffman
/// bit lengths. Returns size of encoded tree and data in bits, not including the
/// 3-bit block header.
fn get_dynamic_lengths(lz77: &Lz77Store, lstart: usize, lend: usize) -> (f64, Vec<u32>, Vec<u32>) {
    let (mut ll_counts, d_counts) = lz77.get_histogram(lstart, lend);
    ll_counts[256] = 1;  /* End symbol. */

    let ll_lengths = length_limited_code_lengths(&ll_counts, 15);
    let mut d_lengths = length_limited_code_lengths(&d_counts, 15);

    patch_distance_codes_for_buggy_decoders(&mut d_lengths[..]);

    try_optimize_huffman_for_rle(lz77, lstart, lend, &ll_counts, &d_counts, ll_lengths, d_lengths)
}

/// Adds all lit/len and dist codes from the lists as huffman symbols. Does not add
/// end code 256. `expected_data_size` is the uncompressed block size, used for
/// assert, but you can set it to `0` to not do the assertion.
fn add_lz77_data<W>(lz77: &Lz77Store, lstart: usize, lend: usize, expected_data_size: usize , ll_symbols: &[u32], ll_lengths: &[u32], d_symbols: &[u32], d_lengths: &[u32], bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    let mut testlength = 0;

    for &item in &lz77.litlens[lstart..lend] {
        match item {
            LitLen::Literal(lit) => {
                let litlen = lit as usize;
                debug_assert!(litlen < 256);
                debug_assert!(ll_lengths[litlen] > 0);
                try!(bitwise_writer.add_huffman_bits(ll_symbols[litlen], ll_lengths[litlen]));
                testlength += 1;
            },
            LitLen::LengthDist(len, dist) => {
                let litlen = len as usize;
                let lls = get_length_symbol(litlen) as u32;
                let ds = get_dist_symbol(dist as i32) as u32;
                debug_assert!(litlen >= 3 && litlen <= 288);
                debug_assert!(ll_lengths[lls as usize] > 0);
                debug_assert!(d_lengths[ds as usize] > 0);
                try!(bitwise_writer.add_huffman_bits(ll_symbols[lls as usize], ll_lengths[lls as usize]));
                try!(bitwise_writer.add_bits(get_length_extra_bits_value(litlen as i32) as u32, get_length_extra_bits(litlen) as u32));
                try!(bitwise_writer.add_huffman_bits(d_symbols[ds as usize], d_lengths[ds as usize]));
                try!(bitwise_writer.add_bits(get_dist_extra_bits_value(dist as i32) as u32, get_dist_extra_bits(dist as i32) as u32));
                testlength += litlen;
            },
        }
    }
    debug_assert!(expected_data_size == 0 || testlength == expected_data_size);
    Ok(())
}

fn add_lz77_block_auto_type<W>(options: &Options, final_block: bool, in_data: &[u8], lz77: &Lz77Store, lstart: usize, lend: usize, expected_data_size: usize, bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    let uncompressedcost = calculate_block_size(lz77, lstart, lend, BlockType::Uncompressed);
    let mut fixedcost = calculate_block_size(lz77, lstart, lend, BlockType::Fixed);
    let dyncost = calculate_block_size(lz77, lstart, lend, BlockType::Dynamic);

    /* Whether to perform the expensive calculation of creating an optimal block
    with fixed huffman tree to check if smaller. Only do this for small blocks or
    blocks which already are pretty good with fixed huffman tree. */
    let expensivefixed = (lz77.size() < 1000) || fixedcost <= dyncost * 1.1;

    let mut fixedstore = Lz77Store::new();
    if lstart == lend {
        /* Smallest empty block is represented by fixed block */
        try!(bitwise_writer.add_bits(final_block as u32, 1));
        try!(bitwise_writer.add_bits(1, 2));  /* btype 01 */
        try!(bitwise_writer.add_bits(0, 7));  /* end symbol has code 0000000 */
        return Ok(());
    }
    if expensivefixed {
        /* Recalculate the LZ77 with lz77_optimal_fixed */
        let instart = lz77.pos[lstart];
        let inend = instart + lz77.get_byte_range(lstart, lend);

        let mut s = ZopfliBlockState::new(options, instart, inend);
        lz77_optimal_fixed(&mut s, in_data, instart, inend, &mut fixedstore);
        fixedcost = calculate_block_size(&fixedstore, 0, fixedstore.size(), BlockType::Fixed);
    }

    if uncompressedcost < fixedcost && uncompressedcost < dyncost {
        add_lz77_block(options, BlockType::Uncompressed, final_block, in_data, lz77, lstart, lend, expected_data_size, bitwise_writer)
    } else if fixedcost < dyncost {
        if expensivefixed {
            add_lz77_block(options, BlockType::Fixed, final_block, in_data, &fixedstore, 0, fixedstore.size(), expected_data_size, bitwise_writer)
        } else {
            add_lz77_block(options, BlockType::Fixed, final_block, in_data, lz77, lstart, lend, expected_data_size, bitwise_writer)
        }
    } else {
        add_lz77_block(options, BlockType::Dynamic, final_block, in_data, lz77, lstart, lend, expected_data_size, bitwise_writer)
    }
}

/// Calculates block size in bits, automatically using the best btype.
pub fn calculate_block_size_auto_type(lz77: &Lz77Store, lstart: usize, lend: usize) -> f64 {
    let uncompressedcost = calculate_block_size(lz77, lstart, lend, BlockType::Uncompressed);
    /* Don't do the expensive fixed cost calculation for larger blocks that are
     unlikely to use it. */
    let fixedcost = if lz77.size() > 1000 {
        uncompressedcost
    } else {
        calculate_block_size(lz77, lstart, lend, BlockType::Fixed)
    };
    let dyncost = calculate_block_size(lz77, lstart, lend, BlockType::Dynamic);
    uncompressedcost.min(fixedcost).min(dyncost)
}

fn add_all_blocks<W>(splitpoints: &[usize], lz77: &Lz77Store, options: &Options, final_block: bool, in_data: &[u8], bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    let mut last = 0;
    for &item in splitpoints.iter() {
        try!(add_lz77_block_auto_type(options, false, in_data, lz77, last, item, 0, bitwise_writer));
        last = item;
    }
    add_lz77_block_auto_type(options, final_block, in_data, lz77, last, lz77.size(), 0, bitwise_writer)
}

fn blocksplit_attempt<W>(options: &Options, final_block: bool, in_data: &[u8], instart: usize, inend: usize, bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    let mut totalcost = 0.0;
    let mut lz77 = Lz77Store::new();

    /* byte coordinates rather than lz77 index */
    let mut splitpoints_uncompressed = Vec::with_capacity(options.blocksplittingmax as usize);

    blocksplit(options, in_data, instart, inend, options.blocksplittingmax as usize, &mut splitpoints_uncompressed);
    let npoints = splitpoints_uncompressed.len();
    let mut splitpoints = Vec::with_capacity(npoints);

    let mut last = instart;
    for &item in &splitpoints_uncompressed {
        let mut s = ZopfliBlockState::new(options, last, item);

        let store = lz77_optimal(&mut s, in_data, last, item, options.numiterations);
        totalcost += calculate_block_size_auto_type(&store, 0, store.size());

        // ZopfliAppendLZ77Store(&store, &lz77);
        debug_assert!(store.size() > 0);
        for (&litlens, &pos) in store.litlens.iter().zip(store.pos.iter()) {
            lz77.append_store_item(litlens, pos);
        }

        splitpoints.push(lz77.size());

        last = item;
    }

    let mut s = ZopfliBlockState::new(options, last, inend);

    let store = lz77_optimal(&mut s, in_data, last, inend, options.numiterations);
    totalcost += calculate_block_size_auto_type(&store, 0, store.size());

    // ZopfliAppendLZ77Store(&store, &lz77);
    debug_assert!(store.size() > 0);
    for (&litlens, &pos) in store.litlens.iter().zip(store.pos.iter()) {
        lz77.append_store_item(litlens, pos);
    }

    /* Second block splitting attempt */
    if npoints > 1 {
        let mut splitpoints2 = Vec::with_capacity(splitpoints_uncompressed.len());
        let mut totalcost2 = 0.0;

        blocksplit_lz77(options, &lz77, options.blocksplittingmax as usize, &mut splitpoints2);

        let mut last = 0;
        for &item in &splitpoints2 {
            totalcost2 += calculate_block_size_auto_type(&lz77, last, item);
            last = item;
        }
        totalcost2 += calculate_block_size_auto_type(&lz77, last, lz77.size());

        if totalcost2 < totalcost {
            splitpoints = splitpoints2;
        }
    }

    add_all_blocks(&splitpoints, &lz77, options, final_block, in_data, bitwise_writer)
}

/// Since an uncompressed block can be max 65535 in size, it actually adds
/// multible blocks if needed.
fn add_non_compressed_block<W>(final_block: bool, in_data: &[u8], instart: usize, inend: usize, bitwise_writer: &mut BitwiseWriter<W>) -> io::Result<()>
    where W: Write
{
    let in_data = &in_data[instart..inend];

    for (chunk, is_final) in in_data.chunks(65535).is_final() {
        let blocksize = chunk.len();
        let nlen = !blocksize;

        try!(bitwise_writer.add_bit((final_block && is_final) as u8));
        /* BTYPE 00 */
        try!(bitwise_writer.add_bit(0));
        try!(bitwise_writer.add_bit(0));

        try!(bitwise_writer.finish_partial_bits());

        try!(bitwise_writer.add_byte((blocksize % 256) as u8));
        try!(bitwise_writer.add_byte(((blocksize / 256) % 256) as u8));
        try!(bitwise_writer.add_byte((nlen % 256) as u8));
        try!(bitwise_writer.add_byte(((nlen / 256) % 256) as u8));

        try!(bitwise_writer.add_bytes(chunk));
    }

    Ok(())
}

pub struct BitwiseWriter<W> {
    bit: u8,
    bp: u8,
    len: usize,
    out: W,
}

impl<W> BitwiseWriter<W>
    where W: Write
{
    fn new(out: W) -> BitwiseWriter<W> {
        BitwiseWriter {
            bit: 0,
            bp: 0,
            len: 0,
            out: out,
        }
    }

    fn bytes_written(&self) -> usize {
        self.len + if self.bp > 0 { 1 } else { 0 }
    }

    /// For when you want to add a full byte.
    fn add_byte(&mut self, byte: u8) -> io::Result<()> {
        self.add_bytes(&[byte])
    }

    /// For adding a slice of bytes.
    fn add_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.len += bytes.len();
        self.out.write_all(bytes)
    }

    fn add_bit(&mut self, bit: u8) -> io::Result<()> {
        self.bit |= bit << self.bp;
        self.bp += 1;
        if self.bp == 8 {
            self.finish_partial_bits()
        } else {
            Ok(())
        }
    }

    fn add_bits(&mut self, symbol: u32, length: u32) -> io::Result<()> {
        // TODO: make more efficient (add more bits at once)
        for i in 0..length {
            let bit = ((symbol >> i) & 1) as u8;
            try!(self.add_bit(bit));
        }

        Ok(())
    }

    /// Adds bits, like `add_bits`, but the order is inverted. The deflate specification
    /// uses both orders in one standard.
    fn add_huffman_bits(&mut self, symbol: u32, length: u32) -> io::Result<()> {
        // TODO: make more efficient (add more bits at once)
        for i in 0..length {
            let bit = ((symbol >> (length - i - 1)) & 1) as u8;
            try!(self.add_bit(bit));
        }

        Ok(())
    }

    fn finish_partial_bits(&mut self) -> io::Result<()> {
        if self.bp != 0 {
            let bytes = &[self.bit];
            try!(self.add_bytes(bytes));
            self.bit = 0;
            self.bp = 0;
        }
        Ok(())
    }
}

fn set_counts_to_count(counts: &mut [usize], count: usize, i: usize, stride: usize) {
    for c in &mut counts[(i - stride)..i] {
        *c = count;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_set_counts_to_count() {
        let mut counts = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let count = 100;
        let i = 8;
        let stride = 5;

        set_counts_to_count(&mut counts, count, i, stride);

        assert_eq!(
            counts,
            vec![0, 1, 2, 100, 100, 100, 100, 100, 8, 9]
        )
    }
}
