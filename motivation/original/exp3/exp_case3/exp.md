Refer `../general_manifest.md` for the general information about the experiment.

1. Definition of culprit
Noise - Loading memory numa node 1 from chiplet 0

Culprit - Running benchmark from chiplet 0 to memory numa 1.

Related Inter chiplet - Running Benchmark from chiplet 1 to memory numa 1

Irrelevant within - Running benchmark from chiplet 0 to memory numa 3

Irrelevant inter chiplet - Running benchmark from chiplet 1 to memory numa 3

X axis - different load (0, 25, 50, 75, 100%)

2. benchmark

* use gapbs bfs and pr on synthetic kronecker scale 20
* Use only a single thread


