// Makers vetted by SwapMarket, by network. To change this list, change this file: its history is the record of who was added and when.
// A key is the maker's full Nostr public key in lowercase hex, as "Maker answering quotes as ..." prints it. Offers are signed
// by that key, so a maker can't take on a listed key without holding it. Being listed says a maker is expected to follow through.
// It doesn't protect funds, because a swap's safety never depends on the maker. Empty until SwapMarket runs a liq2any maker.
export const VETTED_MAKERS = {
  testnet: [],
  mainnet: [],
};

export const vettedMaker = (network, key) => (VETTED_MAKERS[network] ?? []).find((m) => m.key === key) ?? null;

// Whether there is anyone to filter for. With nobody listed, "only vetted makers" would hide every offer.
export const anyVetted = (network) => (VETTED_MAKERS[network] ?? []).length > 0;
