# ys-kcp

Reliable-UDP Library for Rust

## Features

- `tokio` - Enables `tokio` support for the library, allowing it to be used in asynchronous contexts with `await`.
- `byte-check` - Adds an arbitrary field for a packet data hash, making it conformant with anime game's KCP implementation.

## Modifications

- [Changed `KCP_OVERHEAD` to `32`](https://discord.com/channels/965284035985305680/1156564998185816114/1233773942200729600)
- [Added the `xxh3` hash for packet data](https://discord.com/channels/965284035985305680/1156564998185816114/1233773942200729600)
- [Added support for `tokio`](https://git.xeondev.com/reversedrooms/NaviaImpact/src/branch/master/kcp)

## See Other

- https://github.com/skywind3000/kcp - Original KCP protocol
- https://github.com/RustySamovar/kcp - Fork of KCP for _anime game_
- https://github.com/Matrix-Zhang/tokio_kcp - Tokio KCP implementation
- https://git.xeondev.com/reversedrooms/NaviaImpact - Reference for `tokio` support

---

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.