use std::f64;

use deflate::calculate_block_size_auto_type;
use lz77::{Lz77Store, ZopfliBlockState};
use Options;

/// Finds minimum of function `f(i)` where `i` is of type `usize`, `f(i)` is of type
/// `f64`, `i` is in range `start-end` (excluding `end`).
/// Returns the index to the minimum and the minimum value.
fn find_minimum<F>(f: F, start: usize, end: usize) -> (usize, f64)
    where F: Fn(usize) -> f64
{
    let mut start = start;
    let mut end = end;
    if end - start < 1024 {
        let mut best = f64::MAX;
        let mut result = start;
        for i in start..end {
            let v = f(i);
            if v < best {
                best = v;
                result = i;
            }
        }
        (result, best)
    } else {
        /* Try to find minimum faster by recursively checking multiple points. */
        let num = 9;  /* Good value: 9. ?!?!?!?! */
        let mut p;
        let mut best_index;
        let mut best;
        let mut lastbest = f64::MAX;
        let mut pos      = start;
        let mut range    = end - start;

        while range > num {
            // Collect the values for `p`
            p = (1..num+1).map(|i| start + i * (range / num)).collect::<Vec<usize>>();

            best_index = 0;
            best = f(p[0]);

            // Iterate across values in `p` to find the best value and that value's index.
            for (i, item) in p.iter().map(|item| f(*item)).enumerate().take(num).skip(1) {
                if item < best {
                    best = item;
                    best_index = i;
                }
            }

            // Break from the loop if the best value found is greater than the last best value.
            if best > lastbest {
                break;
            }

            if best_index != 0 { start = p[best_index - 1] }
            if best_index != num - 1 { end = p[best_index + 1] }

            pos = p[best_index];
            lastbest = best;
            range = end - start;
        }
        (pos, lastbest)
    }
}

/// Returns estimated cost of a block in bits.  It includes the size to encode the
/// tree and the size to encode all literal, length and distance symbols and their
/// extra bits.
///
/// litlens: lz77 lit/lengths
/// dists: ll77 distances
/// lstart: start of block
/// lend: end of block (not inclusive)
fn estimate_cost(lz77: &Lz77Store, lstart: usize, lend: usize) -> f64 {
    calculate_block_size_auto_type(lz77, lstart, lend)
}

/// Finds next block to try to split, the largest of the available ones.
/// The largest is chosen to make sure that if only a limited amount of blocks is
/// requested, their sizes are spread evenly.
/// lz77size: the size of the LL77 data, which is the size of the done array here.
/// done: array indicating which blocks starting at that position are no longer
///     splittable (splitting them increases rather than decreases cost).
/// splitpoints: the splitpoints found so far.
/// npoints: the amount of splitpoints found so far.
/// lstart: output variable, giving start of block.
/// lend: output variable, giving end of block.
/// returns 1 if a block was found, 0 if no block found (all are done).
fn find_largest_splittable_block(lz77size: usize, done: &[u8], splitpoints: &[usize]) -> Option<(usize, usize)> {
    let mut longest = 0;
    let mut found = None;

    let mut last = 0;

    for &item in splitpoints.iter() {
        if done[last] == 0 && item - last > longest {
            found = Some((last, item));
            longest = item - last;
        }
        last = item;
    }

    let end = lz77size - 1;
    if done[last] == 0 && end - last > longest {
        found = Some((last, end));
    }

    found
}

/// Prints the block split points as decimal and hex values in the terminal.
fn print_block_split_points(lz77: &Lz77Store, lz77splitpoints: &[usize]) {
    let nlz77points = lz77splitpoints.len();
    let mut splitpoints = Vec::with_capacity(nlz77points);

    /* The input is given as lz77 indices, but we want to see the uncompressed
    index values. */
    let mut pos = 0;
    if nlz77points > 0 {
        for i in 0..lz77.size() {
            let length = if lz77.dists[i] == 0 {
                1
            } else {
                lz77.litlens[i]
            };
            if lz77splitpoints[splitpoints.len()] == i {
                splitpoints.push(pos);
                if splitpoints.len() == nlz77points {
                    break;
                }
            }
            pos += length;
        }
    }
    assert!(splitpoints.len() == nlz77points);

    println!("block split points: {} (hex: {})", splitpoints.iter().map(|&sp| format!("{}", sp)).collect::<Vec<_>>().join(" "), splitpoints.iter().map(|&sp| format!("{:x}", sp)).collect::<Vec<_>>().join(" "));
}

/// Does blocksplitting on LZ77 data.
/// The output splitpoints are indices in the LZ77 data.
/// maxblocks: set a limit to the amount of blocks. Set to 0 to mean no limit.
pub fn blocksplit_lz77(options: &Options, lz77: &Lz77Store, maxblocks: usize, splitpoints: &mut Vec<usize>) {

    if lz77.size() < 10 {
        return;  /* This code fails on tiny files. */
    }

    let mut llpos;
    let mut numblocks = 1;
    let mut splitcost;
    let mut origcost;
    let mut done = vec![0; lz77.size()];
    let mut lstart = 0;
    let mut lend = lz77.size();

    while maxblocks != 0 && numblocks < maxblocks {
        assert!(lstart < lend);
        let find_minimum_result = find_minimum(|i| estimate_cost(lz77, lstart, i) + estimate_cost(lz77, i, lend), lstart + 1, lend);
        llpos = find_minimum_result.0;
        splitcost = find_minimum_result.1;

        assert!(llpos > lstart);
        assert!(llpos < lend);

        origcost = estimate_cost(lz77, lstart, lend);

        if splitcost > origcost || llpos == lstart + 1 || llpos == lend {
            done[lstart] = 1;
        } else {
            splitpoints.push(llpos);
            splitpoints.sort();
            numblocks += 1;
        }

        // If `find_largest_splittable_block` returns `None`, no further split will
        // likely reduce compression.
        let is_finished = find_largest_splittable_block(lz77.size(), &done, splitpoints)
            .map_or(true, |(start, end)| {
                lstart = start;
                lend = end;
                lend - lstart < 10
            });

        if is_finished { break }
    }

    if options.verbose {
        print_block_split_points(lz77, splitpoints);
    }
}

/// Does blocksplitting on uncompressed data.
/// The output splitpoints are indices in the uncompressed bytes.
///
/// options: general program options.
/// in: uncompressed input data
/// instart: where to start splitting
/// inend: where to end splitting (not inclusive)
/// maxblocks: maximum amount of blocks to split into, or 0 for no limit
/// splitpoints: dynamic array to put the resulting split point coordinates into.
///   The coordinates are indices in the input array.
/// npoints: pointer to amount of splitpoints, for the dynamic array. The amount of
///   blocks is the amount of splitpoitns + 1.
pub fn blocksplit(options: &Options, in_data: &[u8], instart: usize, inend: usize, maxblocks: usize, splitpoints: &mut Vec<usize>) {
    let mut lz77splitpoints = Vec::with_capacity(maxblocks);
    let mut store = Lz77Store::new();
    splitpoints.clear();

    /* Unintuitively, Using a simple LZ77 method here instead of lz77_optimal
    results in better blocks. */
    {
        let mut state = ZopfliBlockState::new_without_cache(options, instart, inend);
        store.greedy(&mut state, in_data, instart, inend);
    }

    blocksplit_lz77(options, &store, maxblocks, &mut lz77splitpoints);

    let nlz77points = lz77splitpoints.len();

    /* Convert LZ77 positions to positions in the uncompressed input. */
    let mut pos = instart;
    if nlz77points > 0 {
        for i in 0..store.size() {
            let length = if store.dists[i] == 0 { 1 } else { store.litlens[i] };
            if lz77splitpoints[splitpoints.len()] == i {
                splitpoints.push(pos);
                if splitpoints.len() == nlz77points {
                    break;
                }
            }
            pos += length as usize;
        }
    }
    assert!(splitpoints.len() == nlz77points);
}
