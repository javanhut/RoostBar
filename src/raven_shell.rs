//! Client bindings for `raven_shell_v1`, generated from the copy of the XML
//! in `protocols/`. The file is a copy rather than a path into the RavenGUI
//! tree because the bar is built on its own; the protocol's stability note
//! (additive changes only) is what makes keeping two copies safe.
//!
//! The bar uses exactly one request, `open_quick_settings`, which arrived
//! with interface version 2. An older Huginn advertises version 1 and the
//! bar treats that as "no global": it binds only when the compositor can
//! actually honour the request rather than sending one the compositor would
//! kill the connection over.
//!
//! The generated code reaches its dependencies through `super::`, so it has
//! to live one module down from the `use` lines it expects to find there.

#![allow(non_upper_case_globals, dead_code, unused_imports, clippy::single_component_path_imports)]

pub use generated::*;

mod generated {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/raven-shell-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/raven-shell-v1.xml");
}
