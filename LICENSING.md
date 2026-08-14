# Licensing

This repository contains material with different provenance and is not licensed as
a whole under a single license. A license identified below applies only to the
files and rights expressly covered by that section. It does not grant rights in
third-party material.

## WGSL shaders authored by the project author — MIT

The following files are licensed under the MIT License reproduced below:

- `src/lock/animations/creeper.wgsl`
- `src/lock/animations/torch.wgsl`

This grant covers the shader source code only. It does not grant rights in any
Minecraft names, characters, textures, artwork, models, resource archives, WASM
binaries, trademarks, or other third-party material used with the shaders.

Copyright (c) 2026 dongdigua

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and 
associated documentation files (the "Software"), to deal in the Software without restriction, including 
without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell 
copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the 
following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial 
portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT 
LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO 
EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER 
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE 
USE OR OTHER DEALINGS IN THE SOFTWARE.

## sudo-rs-derived PAM code — MIT

Parts of `src/lock/pam/` are derived from sudo-rs 0.2.14 and are used under the
MIT License. Their provenance, copyright notices, and applicable license text
are provided in:

- [`src/lock/pam/THIRD_PARTY_NOTICES.md`](src/lock/pam/THIRD_PARTY_NOTICES.md)
- [`src/lock/pam/sudo-rs-LICENSE-MIT`](src/lock/pam/sudo-rs-LICENSE-MIT)

That MIT license applies to the identified sudo-rs-derived code; it does not
license this repository as a whole.

## Minecraft and other third-party material

The repository tracks only the asset download helper at `assets/download.sh`.
It does not track or distribute the Minecraft Plus WASM or resource archives,
Minecraft client JARs, or the textures extracted from them. Users must run the
helper themselves to obtain those files, which remain ignored by Git.

Minecraft Plus WASM and resource archives, Minecraft client files, textures,
models, artwork, names, characters, and trademarks are not covered by this
repository's licenses. Rights in that material remain with their respective
owners.

The `embed-assets` feature exists for users who independently obtain those
resources and build a local binary. Project releases do not distribute binaries
built with that feature. Neither the feature nor the asset download helper
grants permission to obtain, copy, embed, or redistribute third-party material;
users must comply with the terms applicable to their copies.

## Other files

Except where a file or section of this document expressly states otherwise,
this document grants no license for other files in the repository. Existing
third-party notices and terms continue to apply to their respective material.

Minecraft Plus Wayland is an unofficial fan project and is not affiliated with,
endorsed by, or sponsored by Mojang or Microsoft.
