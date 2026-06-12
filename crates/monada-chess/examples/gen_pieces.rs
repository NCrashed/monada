//! Generate the chess piece KV6 assets — run once, commit the output.
//!
//! The map used to build its pieces procedurally at load time
//! (`model_box(10, 10, height, colour)`); slice 5 of the asset pipeline
//! ships them as real, hand-editable KV6 files under `map/assets/pieces/`.
//! This tool emits the *same* boxes (so the look is unchanged) as the
//! placeholder art, swappable for sculpted models later.
//!
//! ```text
//! cargo run -p monada-chess --example gen_pieces
//! ```
//!
//! Dimensions/colours mirror `map/scripts/main.rhai`'s old `init`: a
//! 10×10×rank-height box, white `0x80F0EAD8` / black `0x8028343C`.

use std::path::Path;

use roxlap_formats::kv6::{serialize, Kv6};

/// `kind` order matches the script: pawn, knight, bishop, rook, queen,
/// king. The height reads the piece's rank, exactly as `model_box` did.
const KINDS: [(&str, u32); 6] = [
    ("pawn", 10),
    ("knight", 14),
    ("bishop", 16),
    ("rook", 14),
    ("queen", 22),
    ("king", 26),
];

/// voxlap-packed `0x80RRGGBB` (high byte = brightness 0x80).
const COLORS: [(&str, u32); 2] = [("white", 0x80F0_EAD8), ("black", 0x8028_343C)];

fn main() -> std::io::Result<()> {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("map/assets/pieces");
    std::fs::create_dir_all(&out)?;

    for (kind, height) in KINDS {
        for (color, col) in COLORS {
            let kv6 = Kv6::solid_box(10, 10, height, col);
            let path = out.join(format!("{kind}_{color}.kv6"));
            std::fs::write(&path, serialize(&kv6))?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}
