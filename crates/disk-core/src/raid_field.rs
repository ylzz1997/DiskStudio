//! GF(2^8) products used for RAID6 parity.
use std::sync::OnceLock;

pub(crate) fn products() -> &'static [[u8; 256]; 256] {
    static PRODUCTS: OnceLock<[[u8; 256]; 256]> = OnceLock::new();
    PRODUCTS.get_or_init(|| {
        let mut products = [[0; 256]; 256];
        for (factor, row) in products.iter_mut().enumerate() {
            for (value, product) in row.iter_mut().enumerate() {
                let mut value = value as u8;
                for bit in 0..8 {
                    if factor & (1 << bit) != 0 {
                        *product ^= value;
                    }
                    value = (value << 1) ^ if value & 0x80 == 0 { 0 } else { 0x1d };
                }
            }
        }
        products
    })
}
