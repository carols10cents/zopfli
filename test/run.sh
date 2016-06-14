#!/bin/bash
set -eu
./zopfli test/data/* && mv test/data/*.gz test/results/ && git diff --exit-code
