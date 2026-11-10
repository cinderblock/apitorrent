# Release signing

`update-signing.pub` here is the public half of apitorrent's update signing key (minisign,
Ed25519, in the format `tauri signer generate` writes). The daemon and the desktop app
compile it in and install only updates signed with the matching private key, which lives
only in this repository's GitHub Actions secrets (`TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`).

While `update-signing.pub` is missing, builds still say when a new version exists, but
won't install one.

Make the key once, with the GitHub CLI signed in to an account that can set the
repository's secrets:

```sh
bun scripts/update-signing-key.ts
```

The script generates the pair, stores the private key and its password as secrets, writes
`update-signing.pub`, and deletes every local copy of the private key. Commit the public
key afterwards. See `plans/update-signing.md` and "Updates" in `docs/architecture.md`.
