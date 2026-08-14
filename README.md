# MCPWL
An unofficial port of Minecraft Plus! to Wayland

## Build and Run
normal build
(Assets are loaded from the directory specified by `MINECRAFT_PLUS_ASSETS`,
or from `./assets` when the variable is not set.)
```bash
cargo b --release
```

embedded build (assets are embedded into binary,
`MINECRAFT_PLUS_ASSETS` still takes precedence over embedded resources.)
(DO NOT DISTRIBUTE BINARIES, see [LICENSING.md](LICENSING.md))
```bash
cargo b --release --features embed-assets
```
