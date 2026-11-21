//! Core Audio endpoint IDs and names shared by Settings and WASAPI capture.

use windows::core::{Result, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    EDataFlow, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

pub struct Endpoint {
    pub id: String,
    pub name: String,
}

/// The caller's thread must have initialized COM.
pub fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

pub fn endpoint_id(device: &IMMDevice) -> Result<String> {
    unsafe { take_string(device.GetId()?) }
}

/// Read and free a string allocated by the Core Audio and property APIs.
unsafe fn take_string(raw: PWSTR) -> Result<String> {
    let value = raw.to_string().map_err(windows::core::Error::from);
    CoTaskMemFree(Some(raw.0.cast()));
    value
}

fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName)?;
        let name = PropVariantToStringAlloc(&value).and_then(|raw| take_string(raw));
        let _ = PropVariantClear(&mut value);
        name
    }
}

pub fn active_endpoints(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Result<Vec<Endpoint>> {
    let mut endpoints = Vec::new();
    unsafe {
        let devices = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        for index in 0..devices.GetCount()? {
            // An endpoint may disappear during enumeration.
            if let Ok(device) = devices.Item(index) {
                if let Ok(id) = endpoint_id(&device) {
                    let name = friendly_name(&device).unwrap_or_default();
                    let name = if name.trim().is_empty() { "Unnamed audio device".into() } else { name };
                    endpoints.push(Endpoint { id, name });
                }
            }
        }
    }
    endpoints.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    // Keep otherwise identical choices distinguishable without exposing endpoint IDs.
    for group in endpoints.chunk_by_mut(|a, b| a.name == b.name) {
        if group.len() > 1 {
            for (index, device) in group.iter_mut().enumerate() {
                device.name = format!("{} ({})", device.name, index + 1);
            }
        }
    }
    Ok(endpoints)
}
