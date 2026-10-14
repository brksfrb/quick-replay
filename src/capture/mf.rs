//! Media Foundation plumbing shared by the video and audio encoders.

use std::mem::ManuallyDrop;
use std::sync::Once;
use windows::core::{Result, GUID, PWSTR};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaType, IMFSample, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFStartup, MFT_FRIENDLY_NAME_Attribute, MFSTARTUP_NOSOCKET, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

pub fn startup() {
    static START: Once = Once::new();
    START.call_once(|| {
        if let Err(err) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
            log::error!("Media Foundation failed to start: {err}");
        }
    });
}

fn variant(vt: windows::Win32::System::Variant::VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

pub fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

pub fn variant_bool(value: bool) -> VARIANT {
    let flag = if value { VARIANT_TRUE } else { windows::Win32::Foundation::VARIANT_FALSE };
    variant(VT_BOOL, VARIANT_0_0_0 { boolVal: flag })
}

pub fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

pub fn media_type(major: &GUID, subtype: &GUID) -> Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType()? };
    unsafe {
        t.SetGUID(&windows::Win32::Media::MediaFoundation::MF_MT_MAJOR_TYPE, major)?;
        t.SetGUID(&windows::Win32::Media::MediaFoundation::MF_MT_SUBTYPE, subtype)?;
    }
    Ok(t)
}

/// A sample backed by a fresh memory buffer holding `data`.
pub fn sample_from_bytes(data: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

/// An empty sample with a buffer of `capacity` bytes, for encoders that don't allocate their own.
pub fn empty_sample(capacity: u32) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(capacity.max(1))?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

pub fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;
        Ok(bytes)
    }
}

pub fn friendly_name(attributes: &IMFAttributes) -> String {
    unsafe {
        let mut raw = PWSTR::null();
        let mut len = 0u32;
        if attributes.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut raw, &mut len).is_err() {
            return "unknown encoder".into();
        }
        let name = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0 as *const _));
        name
    }
}

/// Takes ownership of an MFTEnum result array and frees it.
pub fn collect_activates(array: *mut Option<IMFActivate>, count: u32) -> Vec<IMFActivate> {
    if array.is_null() {
        return Vec::new();
    }
    let list = unsafe {
        let items = std::slice::from_raw_parts_mut(array, count as usize);
        let list = items.iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(array as *const _));
        list
    };
    list
}
