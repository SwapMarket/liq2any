# Running a maker on Debian

For Debian 12 or later, on x86-64, or on arm64 such as a Raspberry Pi 4 or 5 with Raspberry Pi OS 64-bit (Bookworm). A 32-bit OS
won't do. For now a maker runs on the test networks only, Liquid testnet and Polygon Amoy: mainnet waits for the mainnet escrow.

A maker is two programs. The signer holds the keys and checks every request against its own rules; the maker (the daemon) watches
both chains, answers takers over Nostr, and asks the signer to sign. Each runs as a systemd service, as user `liq2any`.

## Install with one script

`install.sh` downloads the newest release, installs it only if SwapMarket's signature checks out, sets up the maker's folder from
the example configs, starts it, and prints the addresses to fund and its Nostr key.

```sh
sudo apt install curl gpgv
wget https://raw.githubusercontent.com/SwapMarket/liq2any/main/deploy/install.sh
sudo bash install.sh test --page-to npub1... --page-relay wss://your.dm.relay
```

`test` is the maker's folder name. `--page-to` and `--page-relay` are optional (see "Pages on your phone"), and so is
`--maker-vault ADDRESS` (see "A maker vault"). With your own Elements node on the same machine, add
`--elements-rpc USER:PASSWORD` (its `rpcuser` and `rpcpassword`), and `--elements-cli PATH` or `--elements-port N` if they aren't
`/usr/local/bin/elements-cli` and the default port. The script checks the node answers on the right chain, points both configs at
it, keeps a public API as the maker's second source, and turns on `fresh_addresses`. If the node isn't answering yet, the maker is
set up but not started. Then back up `/var/lib/liq2any/test/keys`, fund the addresses, set prices in `maker.json`, and restart
the maker. It never touches a maker folder that exists. Updates are `sudo /opt/liq2any/update.sh`.

## Your own Elements node

A maker reads Liquid better from its own node: public APIs rate-limit, and `fresh_addresses` needs one. `install-elements.sh`
downloads Elements, installs it only if the release's signature by Elements' release key checks out, and runs it as
`elementsd.service`, as user `elements`. Run it before `install.sh`, which then takes the password it prints:

```sh
sudo apt install curl gpgv
wget https://raw.githubusercontent.com/SwapMarket/liq2any/main/deploy/install-elements.sh
wget https://raw.githubusercontent.com/SwapMarket/liq2any/main/deploy/elements-release-key.gpg
sudo bash install-elements.sh testnet --datadir /mnt/ssd/elements
```

Leave out `--datadir` for `/var/lib/elements`. A first sync takes hours, or days on a Raspberry Pi. Install the maker meanwhile
with `--no-start`, and start it once the node has caught up: a node still syncing answers with an old chain. The script never
replaces a node already set up. A node you set up yourself needs `txindex=1`, and `validatepegin=0` unless it has a bitcoind.

## Install by hand

```sh
# The latest release's tag, as update.sh finds it. Or set one yourself, such as TAG=v0.01.
TAG=$(wget -qO- "https://api.github.com/repos/SwapMarket/liq2any/releases?per_page=1" | sed -n '/"tag_name"/{s/.*"tag_name": *"\([^"]*\)".*/\1/p;q;}')
ARCH=$(uname -m)   # x86_64, or aarch64 on a Raspberry Pi
mkdir -p ~/liq2any-download && cd ~/liq2any-download
wget "https://github.com/SwapMarket/liq2any/releases/download/$TAG/liq2any-maker-$TAG-$ARCH-linux.tar.gz" \
     "https://github.com/SwapMarket/liq2any/releases/download/$TAG/SHA256SUMS" \
     "https://github.com/SwapMarket/liq2any/releases/download/$TAG/SHA256SUMS.asc" \
     "https://raw.githubusercontent.com/SwapMarket/liq2any/$TAG/deploy/release-key.gpg"
sha256sum release-key.gpg   # must be 28846ef1fcfac9b8b1b5c7b34bf14844db01488436f98928f3a70b2507dbd8b1

# SHA256SUMS is signed by SwapMarket, and lists the archive's checksum.
gpgv --keyring ./release-key.gpg SHA256SUMS.asc SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar xzf liq2any-maker-*-linux.tar.gz
cd liq2any-maker-*/

sudo useradd --system --home /var/lib/liq2any --shell /usr/sbin/nologin liq2any
sudo install -d /opt/liq2any
sudo install -m 755 liq2any-signer liq2any-maker update.sh /opt/liq2any/
sudo install -m 644 release-key.gpg /opt/liq2any/
sudo install -m 644 liq2any-signer@.service liq2any-maker@.service /etc/systemd/system/

# One folder per maker. `test` can be any name.
sudo install -d -m 700 -o liq2any -g liq2any /var/lib/liq2any /var/lib/liq2any/test /var/lib/liq2any/test/keys /var/lib/liq2any/test/run
sudo install -m 600 -o liq2any -g liq2any signer.example.json /var/lib/liq2any/test/signer.json
sudo install -m 600 -o liq2any -g liq2any maker.example.json /var/lib/liq2any/test/maker.json
```

## Keys

In `/var/lib/liq2any/test/keys`, owned by `liq2any`, mode 600. The signer makes any that are missing on its first start.

| File | What it is |
|---|---|
| `liquid-wallet.key`, `liquid-blinding.key` | The Liquid wallet |
| `polygon.key` | The Polygon key: it signs, pays gas and gas drops, and, without a maker vault, holds the USDT0 |
| `swap-seed.key` | Derives each swap's key. Without it, open swaps can't be claimed |
| `nostr.key` | The maker's public identity. Keep it to stay on the vetted list |

Never use the same keys on mainnet and on a test network.

## Starting with new keys

1. Start the services as below. The signer's log gives the addresses to fund:
   `journalctl -u liq2any-signer@test | grep "signer ready"`.
2. Fund them. On the test networks:
   - **Liquid:** L-BTC from the [Liquid testnet faucet](https://liquidtestnet.com/faucet), and testnet USDt, the asset
     `b612eb46…9d73` ("PEGx USDt Testnet"), which the SideSwap wallet swaps for L-BTC on testnet.
   - **Polygon Amoy:** POL from an Amoy faucet, such as [Polygon's](https://faucet.polygon.technology), and tUSDT0, the
     project's stand-in for USDT0 at `0xb4236fbd3bc5791Ee0faB7f065f96F591d33a724`. Its `faucet(uint256)` gives the caller up to
     1,000 a call: use the page's Faucet tab with your own wallet, then send the tUSDT0 to the maker's Polygon address.

   Quotes in a direction wait until the funds that pay it are there. The maker's gas drop sends a little POL with each L→P lock,
   0.1 POL in the example config, so keep POL for that as well as for gas.
3. Find its public key in the maker's log, `maker answering quotes as <key>`. A new key isn't on the page's vetted list, so its
   offers stay hidden unless a taker turns off "Only vetted by SwapMarket", until the key is added to `client/site/js/vetted.js`.

## Start

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now liq2any-signer@test liq2any-maker@test
journalctl -u liq2any-maker@test -f
```

It is running when the log says `maker answering quotes as <key>`, followed by `maker running`.
`/var/lib/liq2any/test/pages.log` holds what an operator needs to see, such as a rollback alarm. Watch it.

## A maker vault

A maker vault (`contracts/polygon/src/MakerVault.sol`, docs/design.md section 2.6) keeps the USDT0 float away from the hot key.
Its cold owner alone withdraws, and the signer's key, as its operator, locks at most a cap per day. The owner deploys it with this
maker's escrow, the owner's own address, the signer's Polygon address as operator, a window in seconds and a cap in units of
10^-6, then funds it with a plain USDT0 transfer. Name it with `--maker-vault ADDRESS` when installing, or as
`"maker_vault": "0x…"` in the `polygon` section of `signer.json`, then restart the signer.

The signer refuses to start with a vault for another escrow, another operator, or a cap of zero. With one, the vault is the
maker's address in every swap, the capacity offered is the vault's USDT0 or the cap left today, whichever is less, and the key
needs only POL. Withdrawing USDT0 is then the owner's job, not the signer's.

## Moving a running maker

Run one copy at a time. Two copies with the same keys could sign conflicting transactions.

1. Stop the old maker and signer.
2. Copy `keys/`, `signer.db` and `maker.db` to the new folder, including any `-wal` and `-shm` files beside them. The signer's
   ledger is what stops it reusing coins and locking twice, so never start from an empty one with keys that have been used.
3. Set ownership: `sudo chown -R liq2any:liq2any /var/lib/liq2any`.

## Pages on your phone

Pages can also reach you as Nostr private messages (NIP-17), which apps such as Amethyst, 0xchat and Primal show. Add this to the
top level of `maker.json`, with your own key and the DM relays your app reads (its kind 10050 list, in the app's relay settings):

```json
"page_to": { "nostr": "npub1…", "relays": ["wss://relay.example"] },
```

Then check it, and restart the maker:

```sh
sudo -u liq2any /opt/liq2any/liq2any-maker test-page /var/lib/liq2any/test/maker.json
sudo systemctl restart liq2any-maker@test
```

Messages come from the maker's own Nostr key, so name it in your app. `pages.log` stays the record: a message no relay takes is
lost. The maker also pages when its wallets run low.

## Several makers on one machine

Each maker is a folder under `/var/lib/liq2any`, with its own keys, configs and databases. Paths in a config are relative to that
config, so the example configs work unchanged in any folder. Two makers must never share keys or a database. They share this
machine's IP, and so its API rate limits.

## Earnings

```sh
sudo -u liq2any /opt/liq2any/liq2any-maker report /var/lib/liq2any/test/maker.json
```

Per day and direction: swaps, volume and spread earned, then the L-BTC spent and the POL given as gas drops, then what the wallets
hold now. It is safe while
the maker runs.

## Withdraw

The maker sends its funds out only to addresses you set in `signer.json`, so nothing that reaches the signer can choose where
they go: a confidential Liquid address (`tlq1…` on testnet) and a Polygon address.

```json
"withdraw_to": { "liquid": "tlq1qq…", "polygon": "0x…" }
```

Restart the signer after setting them. Then, as the user the services run as:

```sh
sudo -u liq2any /opt/liq2any/liq2any-maker withdraw /var/lib/liq2any/test/maker.json liquid usdt 50
sudo -u liq2any /opt/liq2any/liq2any-maker withdraw /var/lib/liq2any/test/maker.json polygon gas all
```

The chain is `liquid` or `polygon`, the asset `usdt` or `gas` (L-BTC or POL), and the amount is in whole units, such as `12.5`,
or `all`. It refuses while any swap is open, since those may need the funds. `pause` stops new swaps while open ones carry on,
and `resume` starts again. `withdraw-usdt.sh` does all of that for USDt and USDT0: it pauses the maker unless you had, waits up
to an hour for open swaps to finish, withdraws, and resumes, even if the withdrawal fails or you press Ctrl-C.

```sh
wget https://raw.githubusercontent.com/SwapMarket/liq2any/main/deploy/withdraw-usdt.sh
sudo bash withdraw-usdt.sh polygon 50 test
```

The signer keeps enough POL to claim each P→L swap it has locked on Liquid and not yet claimed on Polygon, whatever is asked:
without it, the taker would refund and keep the Liquid coin too. With a maker vault, USDT0 is withdrawn by the vault's owner.

## Update

```sh
sudo /opt/liq2any/update.sh          # the newest release
sudo /opt/liq2any/update.sh v0.02    # or a given one
sudo /opt/liq2any/update.sh --rollback
```

It downloads the release, checks it against its `SHA256SUMS`, stops every running maker and its signer, installs the binaries and
service files, and starts them again. Configs, keys and databases are left alone. The old binaries are kept in
`/opt/liq2any/previous` for `--rollback`, and the new example configs go to `/opt/liq2any/examples`. A swap in progress carries on
after the restart.

It installs nothing unless the release's `SHA256SUMS` is signed by the key in `/opt/liq2any/release-key.gpg`, SwapMarket's
`E7947F8DBC700926D8A8175101151A25AAE22F55`. Releases are built on GitHub and signed by hand afterwards, so a new one can be
unsigned for a while, and the script says so. Neither a GitHub account nor its build machines hold the key.

## Back up

`keys/` and both `.db` files. With `fresh_addresses` on, `signer.db` is also how the signer finds the coins at its newer
addresses. Restart both services after changing a config.
