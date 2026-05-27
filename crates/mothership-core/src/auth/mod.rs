//! The app's shared credential store.
//!
//! Providers are runtime-loaded subprocess adapters that authorize themselves;
//! the core owns nothing provider-specific. The one auth-related thing it does
//! own is a single shared vault where adapter settings and secrets live, keyed
//! by provider — the host loads them on spawn and adapters push fresh tokens
//! back into it. See [`FileCredentialVault`].

mod vault;

pub use vault::FileCredentialVault;
