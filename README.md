# Itchy Sats

[![Bors enabled](https://bors.tech/images/badge_small.svg)](https://app.bors.tech/repositories/39253)
![Build Status](https://github.com/itchysats/itchysats/actions/workflows/ci.yml/badge.svg)

CFD trading on Bitcoin - non-custodial, peer-to-peer, Bitcoin only.

Trading on ItchySats is secured by DLCs (Discreet Log Contracts).
Here is some material to read up on DLCs and ItchySats' protocol implementation:

- [The ItchySats Roadmap](https://itchysats.medium.com/itchysats-roadmap-to-the-most-awesome-bitcoin-dex-464a42bf4881) which includes a simplified example of how the protocol works
- [Blogpost](https://comit.network/blog/2022/01/11/cfd-protocol-explained) by @luckysori of the COMIT team, describing the protocol in detail
- [DLC overview](https://bitcoinops.org/en/topics/discreet-log-contracts/) on Bitcoin Optech for information beyond ItchySats

## Quickstart: Users / Traders

This guide is for how to use ItchySats as a taker.
Currently, ItchySats does not officially support multiple makers.
Please note that trade execution is already fully peer to peer, but there is currently only one maker out there.
We are working on adding support for maker discovery and trading with multiple makers for a more competitive market.

ItchySats is available in the [Umbrel](getumbrel.com/) appstore.
We recommend using ItchySats on Umbrel for the time being.

If you want to try ItchySats without Umbrel you can use the [latest binary](github.com/itchysats/itchysats/releases/latest) or [latest docker container](https://github.com/itchysats/itchysats/pkgs/container/itchysats%2Ftaker) but that might not be as straightforward.

To open a position:

1. Install the App on Umbrel (or start the binary / container)
2. Transfer funds into the ItchySats wallet
3. Open a `long` or `short` position - this will lock the funds in a multisig on chain
4. Wait for the price to move
5. Close the position - this will spend the locked up funds according to the price

With the current maker you can close positions at any point in time.
Trades are limited to a contract size between `100` and `1000` contracts.

If the market maker is not available you can close by using an independent oracle attesting to the price.
Oracle price outcomes can be found [here](https://outcome.observer/h00.ooo/x/BitMEX/BXBT).

**Can't find what you are looking for? Check out the [👉 FAQ 👈](http://faq.itchysats.network).**

### ItchySats Wallet

ItchySats includes an internal wallet that is used to sign transactions during the DLC setup.
Additionally, when a CFD is closed, your payout is sent to an address owned by this wallet.
This wallet is completely under your control.
You can withdraw from the wallet at any time.

On Umbrel this wallet is derived from the Umbrel Seed, so the only thing you have to back up is the Umbrel seed.

When running the binary / docker container a random seed will be used to derive the wallet.
Make sure to back up the `taker_seed` file that can be found in the data directory of the application.

### Safety

ItchySats is currently Beta software.
We are doing our best to make ItchySats stable, but there could be unexpected bugs that result in positions being closed at an unfavorable point in time.
Please be mindful of how much money you transfer to the internal wallet and how much you are willing to risk when opening a new CFD.

## Quickstart: Developers

To start the local dev-environment all the components can be started at once by running the following script:

```bash
./start_all.sh
```

Note: Before first run, you need to run `cd maker-frontend; yarn install; cd../taker-frontend; yarn install` command to ensure that all dependencies get
installed.

The script combines the logs from all binaries inside a single terminal so it
might not be ideal for all cases, but it is convenient for quick regression testing.

Pressing `Ctrl + c` once stops all the processes.

The script also enables backtraces by setting `RUST_BACKTRACE=1` env variable.

The maker and taker frontend depend on the respective daemon running.

### Starting the maker and taker frontend

We use a separate react projects for hosting taker and maker frontends.

#### Building the frontends

The latest version of the built frontends will be embedded by `cargo` inside
their respective daemons and automatically served when the daemon starts.
Embedded frontend is served on ports `8000` and `8001` by default.

This means that it is highly recommended to build the frontend _before_ the daemons.

##### Taker

```bash
cd taker-frontend
yarn install
yarn build
```

##### Maker

```bash
cd maker-frontend
yarn install
yarn build
```

#### Developing frontend code

If hot-reloading of the app is required, frontend can be started in development mode.
Development frontend is served on ports `3000` and `3001` by default.

##### Taker

```bash
cd taker-frontend
yarn install
yarn dev
```

##### Maker

```bash
cd maker-frontend
yarn install
yarn dev
```

#### Linting

To run eslint, use:

```bash
cd maker-frontend && yarn run eslint
cd taker-frontend && yarn run eslint
```

## Contact

Feel free to reach out to us on [Twitter](twitter.com/itchysats), [Telegram](https://t.me/joinchat/ULycH50PLV1jOTI0) or [Matrix](https://matrix.to/#/!OSErkwZgvuIhcizfaI:matrix.org?via=matrix.org).

## Contributing

We encourage community contributions whether it be a bug fix or an improvement to the documentation.
Please have a look at the [contributing guidelines](./CONTRIBUTING.md).
