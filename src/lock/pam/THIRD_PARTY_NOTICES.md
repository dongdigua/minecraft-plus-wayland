# Third-party notices

## sudo-rs PAM implementation

Parts of `src/lock/pam/` are derived from the Linux-PAM and secure-memory
implementations in
[`sudo-rs`](https://github.com/trifectatechfoundation/sudo-rs), version 0.2.14.
The imported implementation was reduced to the Linux-PAM ABI and rewritten for
a non-interactive, one-password session-lock conversation.

Copyright (c) 2022-2026 Trifecta Tech Foundation and contributors  
Copyright (c) 1994-1996, 1998-2024 Todd C. Miller

The derived portions are used under the MIT license. The complete license text
is distributed at [`sudo-rs-LICENSE-MIT`](sudo-rs-LICENSE-MIT).

No license for the Minecraft Plus Wayland project as a whole is granted or
changed by this third-party notice.
