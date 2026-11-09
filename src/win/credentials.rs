//! Streamable login in the current Windows user's Credential Manager.

use super::wide;
use crate::streamable::Login;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::*;

const TARGET: &str = "QuickReplay/Streamable";

pub fn read() -> Result<Option<Login>, String> {
    let target = wide(TARGET);
    let mut ptr = std::ptr::null_mut();
    unsafe {
        if let Err(err) = CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut ptr) {
            return if err.code() == ERROR_NOT_FOUND.to_hresult() {
                Ok(None)
            } else {
                Err("Couldn't read the saved Streamable login. Save it again in Settings.".into())
            };
        }
        let credential = &*ptr;
        let email = credential.UserName.to_string();
        let password = if credential.CredentialBlobSize == 0 {
            Ok(String::new())
        } else {
            // Copy bytes: CredentialBlob is not guaranteed to be aligned as UTF-16.
            let bytes = std::slice::from_raw_parts(credential.CredentialBlob, credential.CredentialBlobSize as usize);
            let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
            String::from_utf16(&units)
        };
        CredFree(ptr.cast());
        let email = email.map_err(|_| "Couldn't read the saved Streamable email.")?;
        let password = password.map_err(|_| "Couldn't read the saved Streamable password.")?;
        Ok(Some(Login { email, password }))
    }
}

pub fn save(email: &str, password: &str) -> Result<(), String> {
    let email = email.trim();
    if email.is_empty() || password.is_empty() || email.contains([':', '\0', '\r', '\n']) || password.contains('\0') {
        return Err("Enter your Streamable email and password.".into());
    }
    let mut target = wide(TARGET);
    let mut username = wide(email);
    let mut blob: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    if blob.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
        return Err("That password is too long to save.".into());
    }
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        UserName: PWSTR(username.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&credential, 0) }.map_err(|_| "Couldn't save the Streamable login in Windows.".into())
}

pub fn clear() -> Result<(), String> {
    let target = wide(TARGET);
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(()),
        Err(err) if err.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
        Err(_) => Err("Couldn't clear the saved Streamable login.".into()),
    }
}
