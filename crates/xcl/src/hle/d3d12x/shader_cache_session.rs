//! Fallback in-memory `ID3D12ShaderCacheSession` when the PC runtime refuses
//! `ID3D12Device9::CreateShaderCacheSession` (e.g. `E_INVALIDARG` / `E_NOTIMPL` under
//! vkd3d-proton or with unsupported flags, #1108).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{Interface, GUID, IUnknown};
use windows::Win32::Graphics::Direct3D12::*;

use crate::util::{E_INVALIDARG, E_NOINTERFACE, E_NOT_SUFFICIENT_BUFFER, E_POINTER, HRESULT, S_FALSE, S_OK};

const DXGI_ERROR_NOT_FOUND: HRESULT = 0x887A_0002u32 as i32;
/// Cap per fallback session store so an unbounded stream of blobs cannot exhaust RAM.
const MAX_STORE_BYTES: usize = 32 << 20;

struct Store {
    entries: HashMap<Vec<u8>, Vec<u8>>,
    bytes: usize,
    delete_on_destroy: bool,
}

impl Store {
    fn new() -> Self {
        Self { entries: HashMap::new(), bytes: 0, delete_on_destroy: false }
    }

    fn insert(&mut self, key: Vec<u8>, val: Vec<u8>) {
        let cost = key.len().saturating_add(val.len());
        let old_cost = self.entries.get(&key).map_or(0, |old| key.len() + old.len());
        let next_bytes = self.bytes.saturating_sub(old_cost).saturating_add(cost);
        if next_bytes > MAX_STORE_BYTES {
            return;
        }
        self.bytes = next_bytes;
        self.entries.insert(key, val);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

fn disk_stores() -> &'static Mutex<HashMap<(u128, u64), Arc<Mutex<Store>>>> {
    static S: OnceLock<Mutex<HashMap<(u128, u64), Arc<Mutex<Store>>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

#[repr(C)]
struct Session {
    vtbl: *const [usize; 12],
    refs: AtomicU32,
    device: *mut c_void,
    desc: D3D12_SHADER_CACHE_SESSION_DESC,
    store: Arc<Mutex<Store>>,
    shared_key: Option<(u128, u64)>,
}

fn vtbl() -> &'static [usize; 12] {
    static V: OnceLock<[usize; 12]> = OnceLock::new();
    V.get_or_init(|| [
        qi as usize,
        add_ref as usize,
        release as usize,
        get_private_data as usize,
        set_private_data as usize,
        set_private_data_interface as usize,
        set_name as usize,
        get_device as usize,
        find_value as usize,
        store_value as usize,
        set_delete_on_destroy as usize,
        get_desc as usize,
    ])
}

fn answers(iid: &GUID) -> bool {
    [IUnknown::IID, ID3D12Object::IID, ID3D12DeviceChild::IID, ID3D12ShaderCacheSession::IID].contains(iid)
}

/// Create an in-memory `ID3D12ShaderCacheSession` for `device` when the PC runtime rejected
/// `CreateShaderCacheSession(desc, riid, ppv)`.
pub(super) unsafe fn create_fallback(
    device: *mut c_void,
    desc: *const D3D12_SHADER_CACHE_SESSION_DESC,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    if desc.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = &*riid;
    if !answers(iid) {
        return E_NOINTERFACE;
    }
    if ppv.is_null() {
        return S_FALSE;
    }
    let d = desc.read_unaligned();
    let (store, shared_key) = if d.Mode == D3D12_SHADER_CACHE_MODE_DISK {
        let key = (d.Identifier.to_u128(), d.Version);
        let mut g = disk_stores().lock().unwrap();
        let s = g.entry(key).or_insert_with(|| Arc::new(Mutex::new(Store::new()))).clone();
        (s, Some(key))
    } else {
        (Arc::new(Mutex::new(Store::new())), None)
    };
    if !device.is_null() {
        if let Some(dev) = ID3D12Device::from_raw_borrowed(&device) {
            std::mem::forget(dev.clone());
        }
    }
    let obj = Box::new(Session {
        vtbl: vtbl(),
        refs: AtomicU32::new(1),
        device,
        desc: d,
        store,
        shared_key,
    });
    *ppv = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "system" fn qi(this: *mut Session, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if riid.is_null() {
        return E_INVALIDARG;
    }
    if answers(&*riid) {
        (*this).refs.fetch_add(1, Ordering::AcqRel);
        *ppv = this as *mut c_void;
        return S_OK;
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut Session) -> u32 {
    (*this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn release(this: *mut Session) -> u32 {
    let prev = (*this).refs.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        let Session { device, store, shared_key, .. } = *Box::from_raw(this);
        match shared_key {
            Some(k) => {
                let mut map = disk_stores().lock().unwrap();
                let same = map.get(&k).is_some_and(|m| Arc::ptr_eq(m, &store));
                drop(store);
                if same && map.get(&k).is_some_and(|m| Arc::strong_count(m) == 1 && m.lock().unwrap().delete_on_destroy) {
                    if let Some(removed) = map.remove(&k) {
                        removed.lock().unwrap().clear();
                    }
                }
            }
            None => {
                let mut g = store.lock().unwrap();
                if g.delete_on_destroy {
                    g.clear();
                }
            }
        }
        if !device.is_null() {
            drop(ID3D12Device::from_raw(device));
        }
        return 0;
    }
    prev - 1
}

unsafe extern "system" fn get_private_data(_this: *mut Session, _guid: *const GUID, size: *mut u32, _data: *mut c_void) -> HRESULT {
    if !size.is_null() {
        *size = 0;
    }
    DXGI_ERROR_NOT_FOUND
}

unsafe extern "system" fn set_private_data(_this: *mut Session, _guid: *const GUID, _size: u32, _data: *const c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn set_private_data_interface(_this: *mut Session, _guid: *const GUID, _data: *const c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn set_name(_this: *mut Session, _name: *const u16) -> HRESULT {
    S_OK
}

unsafe extern "system" fn get_device(this: *mut Session, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if riid.is_null() || (*this).device.is_null() {
        return E_INVALIDARG;
    }
    let dev_vt = *((*this).device as *const *const usize);
    let dev_qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*dev_vt);
    dev_qi((*this).device, riid, ppv)
}

unsafe extern "system" fn find_value(
    this: *mut Session,
    key: *const c_void,
    key_size: u32,
    val: *mut c_void,
    val_size: *mut u32,
) -> HRESULT {
    if key.is_null() || key_size == 0 || val_size.is_null() {
        return E_INVALIDARG;
    }
    let k = std::slice::from_raw_parts(key as *const u8, key_size as usize);
    let g = (*this).store.lock().unwrap();
    let Some(found) = g.entries.get(k) else {
        return DXGI_ERROR_NOT_FOUND;
    };
    let need = found.len() as u32;
    if val.is_null() {
        *val_size = need;
        return S_OK;
    }
    if *val_size < need {
        *val_size = need;
        return E_NOT_SUFFICIENT_BUFFER;
    }
    if need > 0 {
        std::ptr::copy_nonoverlapping(found.as_ptr(), val as *mut u8, found.len());
    }
    *val_size = need;
    S_OK
}

unsafe extern "system" fn store_value(
    this: *mut Session,
    key: *const c_void,
    key_size: u32,
    val: *const c_void,
    val_size: u32,
) -> HRESULT {
    if key.is_null() || key_size == 0 || (val.is_null() && val_size > 0) {
        return E_INVALIDARG;
    }
    let k = std::slice::from_raw_parts(key as *const u8, key_size as usize).to_vec();
    let v = if val_size == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(val as *const u8, val_size as usize).to_vec()
    };
    (*this).store.lock().unwrap().insert(k, v);
    S_OK
}

unsafe extern "system" fn set_delete_on_destroy(this: *mut Session) {
    (*this).store.lock().unwrap().delete_on_destroy = true;
}

unsafe extern "system" fn get_desc(
    this: *mut Session,
    out: *mut D3D12_SHADER_CACHE_SESSION_DESC,
) -> *mut D3D12_SHADER_CACHE_SESSION_DESC {
    if !out.is_null() {
        out.write((*this).desc);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;

    #[test]
    fn rejected_create_shader_cache_session_falls_back_to_working_in_memory_session() {
        unsafe {
            let Ok(dev) = crate::gpu::agility::create_device(None, D3D_FEATURE_LEVEL_11_0) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            if !super::super::real::is_adopted(dev.as_raw()) {
                super::super::real::adopt(dev.as_raw(), super::super::real::Kind::Device);
            }
            let dev9: ID3D12Device9 = match dev.cast() {
                Ok(d) => d,
                Err(_) => return crate::gpu::recomp::runner::skip_test("no GPU"),
            };
            // Bogus flags cause native D3D12 CreateShaderCacheSession to fail with E_INVALIDARG
            // (matching the vkd3d-proton failure in #1108); the hook falls back to an in-memory session.
            let id = GUID::from_u128(0xa1b2c3d4_e5f6_4789_8abc_def012345678);
            let desc = D3D12_SHADER_CACHE_SESSION_DESC {
                Identifier: id,
                Mode: D3D12_SHADER_CACHE_MODE_DISK,
                Flags: D3D12_SHADER_CACHE_FLAGS(0x4000_0000),
                MaximumInMemoryCacheSizeBytes: 1024,
                MaximumInMemoryCacheEntries: 16,
                MaximumValueFileSizeBytes: 4096,
                Version: 7,
            };
            let mut session: Option<ID3D12ShaderCacheSession> = None;
            dev9.CreateShaderCacheSession(&desc, &mut session).expect("fallback session succeeds");
            let session = session.unwrap();
            let got = session.GetDesc();
            assert_eq!(got.Identifier, id);
            assert_eq!(got.Version, 7);

            // Missing entry returns DXGI_ERROR_NOT_FOUND.
            let key = b"pso-key-1";
            let mut sz = 0u32;
            let miss = session.FindValue(key.as_ptr() as *const c_void, key.len() as u32, std::ptr::null_mut(), &mut sz);
            assert_eq!(miss.unwrap_err().code().0, DXGI_ERROR_NOT_FOUND);

            // StoreValue + two-call FindValue (size query, small buffer, full readback).
            let blob = b"cached-dxil-payload";
            session.StoreValue(key.as_ptr() as *const c_void, key.len() as u32, blob.as_ptr() as *const c_void, blob.len() as u32).unwrap();
            session.FindValue(key.as_ptr() as *const c_void, key.len() as u32, std::ptr::null_mut(), &mut sz).unwrap();
            assert_eq!(sz, blob.len() as u32);

            let mut small = [0u8; 4];
            let mut small_sz = small.len() as u32;
            let too_small = session.FindValue(key.as_ptr() as *const c_void, key.len() as u32, small.as_mut_ptr() as *mut c_void, &mut small_sz);
            assert_eq!(too_small.unwrap_err().code().0, E_NOT_SUFFICIENT_BUFFER);
            assert_eq!(small_sz, blob.len() as u32);

            let mut out = vec![0u8; sz as usize];
            session.FindValue(key.as_ptr() as *const c_void, key.len() as u32, out.as_mut_ptr() as *mut c_void, &mut sz).unwrap();
            assert_eq!(out, blob);

            // GetDevice returns the parent device; SetDeleteOnDestroy drops the disk-mode store on Release.
            let mut parent: Option<ID3D12Device> = None;
            session.GetDevice(&mut parent).unwrap();
            assert_eq!(parent.unwrap().as_raw(), dev.as_raw());

            session.SetDeleteOnDestroy();
            drop(session);
            assert!(!disk_stores().lock().unwrap().contains_key(&(id.to_u128(), 7)));
        }
    }

    unsafe fn fallback_session(desc: &D3D12_SHADER_CACHE_SESSION_DESC) -> ID3D12ShaderCacheSession {
        let mut raw: *mut c_void = std::ptr::null_mut();
        assert_eq!(create_fallback(std::ptr::null_mut(), desc, &ID3D12ShaderCacheSession::IID, &mut raw), S_OK);
        ID3D12ShaderCacheSession::from_raw(raw)
    }

    #[test]
    fn shared_disk_store_waits_for_last_session_before_honoring_delete_on_destroy() {
        unsafe {
            let id = GUID::from_u128(0x11223344_5566_4778_899a_bbccddeeff01);
            let desc = D3D12_SHADER_CACHE_SESSION_DESC {
                Identifier: id,
                Mode: D3D12_SHADER_CACHE_MODE_DISK,
                Flags: D3D12_SHADER_CACHE_FLAG_NONE,
                MaximumInMemoryCacheSizeBytes: 1024,
                MaximumInMemoryCacheEntries: 16,
                MaximumValueFileSizeBytes: 4096,
                Version: 19,
            };
            let s1 = fallback_session(&desc);
            let s2 = fallback_session(&desc);

            let key = b"vs-entry";
            let blob = b"dxil-shader-bytes";
            s1.StoreValue(key.as_ptr() as *const c_void, key.len() as u32, blob.as_ptr() as *const c_void, blob.len() as u32).unwrap();

            let mut out = vec![0u8; blob.len()];
            let mut sz = out.len() as u32;
            s2.FindValue(key.as_ptr() as *const c_void, key.len() as u32, out.as_mut_ptr() as *mut c_void, &mut sz).unwrap();
            assert_eq!(out, blob);

            // Marking delete-on-destroy on s1 and closing s1 must keep the shared store intact while s2 lives.
            s1.SetDeleteOnDestroy();
            drop(s1);

            out.fill(0);
            sz = out.len() as u32;
            s2.FindValue(key.as_ptr() as *const c_void, key.len() as u32, out.as_mut_ptr() as *mut c_void, &mut sz)
                .expect("shared store stays populated while another session is open");
            assert_eq!(out, blob);

            let s3 = fallback_session(&desc);
            out.fill(0);
            sz = out.len() as u32;
            s3.FindValue(key.as_ptr() as *const c_void, key.len() as u32, out.as_mut_ptr() as *mut c_void, &mut sz)
                .expect("new session opened before last release shares the same store");
            assert_eq!(out, blob);
            drop(s3);

            // Closing the last open session (even one that did not call SetDeleteOnDestroy itself) deletes the store.
            drop(s2);
            assert!(!disk_stores().lock().unwrap().contains_key(&(id.to_u128(), 19)));

            let s4 = fallback_session(&desc);
            let mut miss_sz = 0u32;
            let miss = s4.FindValue(key.as_ptr() as *const c_void, key.len() as u32, std::ptr::null_mut(), &mut miss_sz);
            assert_eq!(miss.unwrap_err().code().0, DXGI_ERROR_NOT_FOUND);
            s4.SetDeleteOnDestroy();
            drop(s4);
        }
    }

    #[test]
    fn oversized_replacement_preserves_existing_entry_and_failed_create_clears_ppv() {
        let mut store = Store::new();
        let key = b"pso-0".to_vec();
        let orig = b"valid-cached-dxil".to_vec();
        let expected_bytes = key.len() + orig.len();
        store.insert(key.clone(), orig.clone());
        assert_eq!(store.bytes, expected_bytes);

        store.insert(key.clone(), vec![0xAB; MAX_STORE_BYTES]);
        assert_eq!(store.entries.get(key.as_slice()).map(Vec::as_slice), Some(orig.as_slice()));
        assert_eq!(store.bytes, expected_bytes);

        unsafe {
            let desc = D3D12_SHADER_CACHE_SESSION_DESC {
                Identifier: GUID::from_u128(0x22334455_6677_4889_9aab_ccddeeff0012),
                Mode: D3D12_SHADER_CACHE_MODE_MEMORY,
                ..Default::default()
            };
            let mut out: *mut c_void = 1usize as *mut c_void;
            assert_eq!(create_fallback(std::ptr::null_mut(), std::ptr::null(), &ID3D12ShaderCacheSession::IID, &mut out), E_INVALIDARG);
            assert!(out.is_null());

            out = 1usize as *mut c_void;
            let bad_iid = GUID::from_u128(1);
            assert_eq!(create_fallback(std::ptr::null_mut(), &desc, &bad_iid, &mut out), E_NOINTERFACE);
            assert!(out.is_null());

            assert_eq!(create_fallback(std::ptr::null_mut(), &desc, &ID3D12ShaderCacheSession::IID, std::ptr::null_mut()), S_FALSE);
        }
    }
}
