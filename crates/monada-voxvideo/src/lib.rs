//! Voxel-video frame-stream codec (DESIGN.md §3.2).
//!
//! An ordered sequence of KV6 deltas (set-spans / del-spans against a
//! base keyframe) played back at a declared FPS — "MP4 / GIF for
//! voxels". Run-length compression over the per-column delta stream.
//! Standalone and reusable outside the engine (e.g. an offline FX
//! renderer).
//!
//! The format spec is owed in `VOXVIDEO.md` before implementation.
//! Skeleton only — lands at **M6** (DESIGN.md §7, §10.2).
#![forbid(unsafe_code)]
