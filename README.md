# Substrate with PBFT

This repo implement PBFT, Tendermint, and Jasmine as alternatives to GRANDPA.

## Usage

1. [Setup Rust](https://docs.substrate.io/main-docs/install/). Add `nightly` toolchain and `wasm` target.
2. Clone repos. (Under the same folder)

```bash
git clone https://github.com/fky2015/finality-tendermint.git
git clone https://github.com/fky2015/finality-pbft.git
git clone https://github.com/fky2015/finality-jasmine.git
git clone https://github.com/fky2015/bit-substrate.git
```

> NOTE: To build `cargo b --bin node-template`, you need `finality-pbft & bit-substrate`.
> To build `cargo b --bin node-tendermint`, you need `finality-tendermint & bit-substrate`.
> To build `cargo b --bin node-jasmine`, you need `finality-jasmine & bit-substrate`
> 
> Place them under a same directory.

3. Build.

```bash
cd bit-substrate
# build PBFT node
cargo build -p node-template
# build Tendermint node
cargo build -p node-tendermint
# build Jasmine node
cargo build -p node-jasmine
```

4. Run node.

```bash
./target/debug/node-template --dev --tmp
# or for Tendermint
./target/debug/node-tendermint --dev --tmp
# or for Jasmine
./terget/debug/node-jasmine --dev --tmp
```

## Caveat

This implementation of the algorithm is experimental and not intended for production use.

The algorithm implementation does not affect upper layers. If you want to verify your 
upper-layer works, you can always first implement it on top of [the original one][substrate-compatible].

## Test script

Test scripts are in the `./pbft-test-scripts/`, `./tendermint-test-scripts/`,  `./jasmine-test-scripts/`.
Run from project root dir such as `bash ./pbft-test-scripts/single.sh`.

## Info

There is a substrate-contracts-node campatible version [bit-substrate-contracts-node][substrate].

This is campatible to [Substrate May 12th, 2022][substrate-compatible].

For the PBFT implementation details, please visit [finality-pbft][pbft].
For the Tendermint implementation details, please visit [finality-tendermint][tendermint]
For the Jasmine implementation details, please visit [finality-jasmine][jasmine]

[substrate]: https://github.com/fky2015/bit-substrate-contracts-node
[pbft]: https://github.com/fky2015/finality-pbft
[tendermint]: https://github.com/fky2015/finality-tendermint
[jasmine]: https://github.com/fky2015/finality-jasmine
[substrate-compatible]: https://github.com/paritytech/substrate/commit/7d233c2446b5a60662400a0a4bcfb78bb3b79ff7 

*In below is the origin README.*

---

# Substrate &middot; [![GitHub license](https://img.shields.io/badge/license-GPL3%2FApache2-blue)](#LICENSE) [![GitLab Status](https://gitlab.parity.io/parity/substrate/badges/master/pipeline.svg)](https://gitlab.parity.io/parity/substrate/pipelines) [![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](docs/CONTRIBUTING.adoc) [![Matrix](https://img.shields.io/matrix/substrate-technical:matrix.org)](https://matrix.to/#/#substrate-technical:matrix.org)

<p align="center">
  <img src="/docs/media/sub.gif">
</p>

Substrate is a next-generation framework for blockchain innovation 🚀.

## Trying it out

Simply go to [docs.substrate.io](https://docs.substrate.io) and follow the
[installation](https://docs.substrate.io/v3/getting-started/overview) instructions. You can
also try out one of the [tutorials](https://docs.substrate.io/tutorials/).

## Contributions & Code of Conduct

Please follow the contributions guidelines as outlined in [`docs/CONTRIBUTING.adoc`](docs/CONTRIBUTING.adoc). In all communications and contributions, this project follows the [Contributor Covenant Code of Conduct](docs/CODE_OF_CONDUCT.md).

## Security

The security policy and procedures can be found in [`docs/SECURITY.md`](docs/SECURITY.md).

## License

- Substrate Primitives (`sp-*`), Frame (`frame-*`) and the pallets (`pallets-*`), binaries (`/bin`) and all other utilities are licensed under [Apache 2.0](LICENSE-APACHE2).
- Substrate Client (`/client/*` / `sc-*`) is licensed under [GPL v3.0 with a classpath linking exception](LICENSE-GPL3).

The reason for the split-licensing is to ensure that for the vast majority of teams using Substrate to create feature-chains, then all changes can be made entirely in Apache2-licensed code, allowing teams full freedom over what and how they release and giving licensing clarity to commercial teams.

In the interests of the community, we require any deeper improvements made to Substrate's core logic (e.g. Substrate's internal consensus, crypto or database code) to be contributed back so everyone can benefit.

