# c0mpute desktop

The desktop app (`apps/desktop`, Electron) bundles the c0mpute CLI and adds
chat, the agent with your MCP servers and skills, the node manager, hosting and
tasks. This page covers how it keeps your account key.

## Account key: OS keychain on the desktop, chat.json for the CLI and TUI

Sign-in is the CLI's device flow (`c0mpute login`, the same as
`c0mpute chat login`): the desktop runs it, you approve the code on c0mpute.com,
and the CLI writes the key to `~/.c0mpute/chat.json` (mode 0600).

- **Desktop:** imports that key into its own store,
  `~/.c0mpute/desktop-account.json` (0600, written atomically), encrypted with
  Electron `safeStorage`, which is backed by the OS keychain (macOS Keychain,
  Windows DPAPI, libsecret or KWallet on Linux). The file holds the URL, the
  ciphertext, the mtime of the chat.json it came from and a short sha256
  fingerprint of the key; never the key itself. After the import the desktop
  reads the key from there, so it keeps working if chat.json is removed.
- **CLI and TUI:** keep reading the 0600 `chat.json` unchanged. They have no
  keychain dependency, so headless nodes and static builds keep working.

Lookup order in the desktop: `C0MPUTE_API_KEY` (with `C0MPUTE_URL`), then the
keychain store, then chat.json.

The import runs at app start and on every read. It happens when the keychain
store is empty or unreadable, or when chat.json was rewritten after the last
import with a different key (for example you ran `c0mpute login` again in a
terminal as another account). A rewrite with the same key changes nothing.

No keychain (a headless box, or Linux with no Secret Service or KWallet, where
Chromium would fall back to a fixed password): the desktop keeps reading
chat.json as before, writes nothing, and Settings says that no OS keychain is
available. Settings shows "key from OS keychain" when the keychain is in use.

## Sign out

Sign out in the desktop signs out of this machine: it deletes the keychain
store, runs `c0mpute chat logout`, and if a key is still in chat.json (the CLI
is missing or failed) removes it from the file directly, keeping the URL.
Otherwise the next read would import the key again. The CLI and TUI are signed
out too, since they share chat.json.
