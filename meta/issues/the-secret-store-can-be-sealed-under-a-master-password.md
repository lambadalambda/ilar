# The secret store can be sealed under a master password

## Summary

Values in `secrets.json` sat in the clear, as safe as the user account.
The person wants them encrypted under one master password, asked for
once per session.

## Requirements

- `ilar secret encrypt` seals the store (asks twice); `ilar secret
  decrypt` writes it back in the clear. Argon2id key derivation,
  XChaCha20-Poly1305, a fresh nonce every write, nothing readable
  without the password, names included.
- One unlock per process: the TUI and `ilar exec` ask on the terminal
  at start and read the configuration again afterwards; `ilar secret`
  asks when it needs to; the gateway takes `/unlock <password>` from a
  chat and logs that it is locked until then.
- Locked, every use of a secret is refused with a note saying so, the
  `secrets` tool says the store is locked, and nothing else breaks.

## Acceptance Criteria

- A sealed file contains none of the plaintext; a wrong password is
  refused; the right one reads and writes through for the process;
  decrypting restores the plain file.
- The CLI, TUI and gateway paths are covered by tests or by hand.

## Notes

- Unattended gateway: the password must be typed after every restart,
  and a provider key in a sealed store is only read after the unlock.
  The docs say to keep provider keys in ilar.toml on such a box.
