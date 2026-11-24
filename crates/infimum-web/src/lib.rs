//! `infimum-web` library crate — powers both the native Vulkan/winit desktop runtime
//! and the `wasm32` WebGL2/WebGPU browser player (`src/web_player.rs`).

pub mod atlas;
#[cfg(not(target_arch = "wasm32"))]
pub mod audio;
pub mod browser_abi;
#[cfg(not(target_arch = "wasm32"))]
pub mod capabilities;
#[cfg(not(target_arch = "wasm32"))]
pub mod clipboard;
pub mod gpu;
#[cfg(not(target_arch = "wasm32"))]
pub mod passport;
pub mod runtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod sanitizer;
pub mod textures3d;

#[cfg(target_arch = "wasm32")]
pub mod web_player;
