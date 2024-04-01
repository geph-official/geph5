# New GUI design

## GUI framework

We absolutely need to move away from the "webpage in a webview" model that we used for Geph4, which was slow, laggy, and somewhat difficult to style in a way that looks like a GUI rather than a webpage. The only advantage was code reuse across platforms, but the complexity required to bootstrap all the pieces together (platform-specific webview code, platform-specific "glue" code, HTML/JS, and a platform-specific `geph4-client`) make it almost not worth it.

Instead, Geph5 will have a [egui](http://egui.rs/), since it's

- completely cross-platform, even to mobile
- very fast, even on low-end devices
- does not have any bloated dependencies
- has a nice minimalistic / retro-ish aesthetic

Also I just really love immediate-mode GUIs.

## RPC design

Like the old GUI design, we use an RPC mechanism to communicate between the GUI and the _steady-state_ business logic. This is the case even in platforms where Geph is a single-process program (desktops), since on iOS (and possibly Android) it's most natural for Geph to run on a different process.

However, RPC mechanisms are not used for _one-off_ business logic, like retrieving the list of exit nodes, logging in, etc. We directly call into code that does these things from the GUI code.

Fortunately, through `nanorpc-sillad` this can be made completely type-safe and natural.
