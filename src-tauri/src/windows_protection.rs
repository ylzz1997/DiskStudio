//! User-scoped Windows protection for disk operation records.
use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    },
};

pub(crate) fn protect(
    input: &[u8],
    seal: bool,
    domain: Option<&[u8]>,
    max_output: u32,
) -> Result<Vec<u8>, ()> {
    let blob = |bytes: &[u8]| -> Result<CRYPT_INTEGER_BLOB, ()> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len()).map_err(|_| ())?,
            pbData: bytes.as_ptr().cast_mut(),
        })
    };
    let input = blob(input)?;
    let domain = domain.map(blob).transpose()?;
    let entropy = domain.as_ref().map_or(null(), std::ptr::from_ref);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    // DPAPI reads the input blobs and allocates its own result.
    let ok = unsafe {
        if seal {
            CryptProtectData(
                &input,
                null(),
                entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    let result = if ok != 0 && !output.pbData.is_null() && (1..=max_output).contains(&output.cbData)
    {
        Ok(unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec())
    } else {
        Err(())
    };
    if !output.pbData.is_null() {
        unsafe { LocalFree(output.pbData.cast()) };
    }
    result
}
