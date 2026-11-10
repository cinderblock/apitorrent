`tauri-cli.pub` and `hello.txt.sig` were made by `cargo tauri signer generate` and
`cargo tauri signer sign` (tauri-cli 2.10.1) with a throwaway key, over the 6 bytes
`hello\n`. They prove the daemon reads what the release workflow's signer writes. The
private half was deleted; it signs nothing else.
