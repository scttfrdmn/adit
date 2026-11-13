# adit documentation

adit moves bytes from Amazon S3 into NVIDIA GPU memory on EC2, with zero copies on the GPU
host. GPU-less EFA proxies fetch from S3 and RDMA-write into the client's registered HBM. It is
a very good solution to a certain shape of problem, and these pages say which shape, measured.

| Page | What it covers |
|---|---|
| [paths.md](paths.md) | Every way to get S3 data into GPU memory on AWS (paths A–G), diagrammed, and the shape of problem adit fits |
| [results.md](results.md) | Measured results so far, with charts drawn from `bench/results/` |

These pages are the start of the project website (#123). Numbers enter them only from
`bench/results/*.csv` rows, each with its configuration. Where adit loses, the page says so.
