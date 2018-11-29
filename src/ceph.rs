// Copyright 2017 LambdaStack All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
#![cfg(target_os = "linux")]
#![allow(unused_imports)]

use JsonData;

use admin_sockets::*;
use byteorder::{LittleEndian, WriteBytesExt};
use error::*;
use json::*;
use libc::*;
use nom::{le_u32, IResult};
use serde_json;
use JsonValue;

use rados::*;
use status::*;
use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::{ptr, str, mem, thread, time};

use std::io::{BufRead, Cursor};
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use utils::*;

use uuid::Uuid;

const CEPH_OSD_TMAP_HDR: char = 'h';
const CEPH_OSD_TMAP_SET: char = 's';
const CEPH_OSD_TMAP_CREATE: char = 'c';
const CEPH_OSD_TMAP_RM: char = 'r';

#[derive(Debug, Clone)]
pub enum CephHealth {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub enum CephCommandTypes {
    Mon,
    Osd,
    Pgs,
}
pub(crate) fn get_error(n: c_int) -> RadosResult<String> {
    let mut buf = vec![0u8; 256];
    unsafe {
        strerror_r(n, buf.as_mut_ptr() as *mut c_char, buf.len());
    }
    buf = buf.iter().take_while(|&x| x != &0u8).cloned().collect();
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

named!(
    parse_header<TmapOperation>,
    do_parse!(
        char!(CEPH_OSD_TMAP_HDR) >> data_len: le_u32 >> data: take!(data_len) >> (TmapOperation::Header {
            data: data.to_vec(),
        })
    )
);

named!(
    parse_create<TmapOperation>,
    do_parse!(
        char!(CEPH_OSD_TMAP_CREATE) >> key_name_len: le_u32 >> key_name: take_str!(key_name_len) >> data_len: le_u32
            >> data: take!(data_len) >> (TmapOperation::Create {
            name: key_name.to_string(),
            data: data.to_vec(),
        })
    )
);

named!(
    parse_set<TmapOperation>,
    do_parse!(
        char!(CEPH_OSD_TMAP_SET) >> key_name_len: le_u32 >> key_name: take_str!(key_name_len) >> data_len: le_u32
            >> data: take!(data_len) >> (TmapOperation::Set {
            key: key_name.to_string(),
            data: data.to_vec(),
        })
    )
);

named!(
    parse_remove<TmapOperation>,
    do_parse!(
        char!(CEPH_OSD_TMAP_RM) >> key_name_len: le_u32 >> key_name: take_str!(key_name_len)
            >> (TmapOperation::Remove {
                name: key_name.to_string(),
            })
    )
);

#[derive(Debug)]
pub enum TmapOperation {
    Header { data: Vec<u8> },
    Set { key: String, data: Vec<u8> },
    Create { name: String, data: Vec<u8> },
    Remove { name: String },
}

impl TmapOperation {
    fn serialize(&self) -> RadosResult<Vec<u8>> {
        let mut buffer: Vec<u8> = Vec::new();
        match *self {
            TmapOperation::Header { ref data } => {
                buffer.push(CEPH_OSD_TMAP_HDR as u8);
                try!(buffer.write_u32::<LittleEndian>(data.len() as u32));
                buffer.extend_from_slice(data);
            }
            TmapOperation::Set { ref key, ref data } => {
                buffer.push(CEPH_OSD_TMAP_SET as u8);
                try!(buffer.write_u32::<LittleEndian>(key.len() as u32));
                buffer.extend(key.as_bytes());
                try!(buffer.write_u32::<LittleEndian>(data.len() as u32));
                buffer.extend_from_slice(data);
            }
            TmapOperation::Create { ref name, ref data } => {
                buffer.push(CEPH_OSD_TMAP_CREATE as u8);
                try!(buffer.write_u32::<LittleEndian>(name.len() as u32));
                buffer.extend(name.as_bytes());
                try!(buffer.write_u32::<LittleEndian>(data.len() as u32));
                buffer.extend_from_slice(data);
            }
            TmapOperation::Remove { ref name } => {
                buffer.push(CEPH_OSD_TMAP_RM as u8);
                try!(buffer.write_u32::<LittleEndian>(name.len() as u32));
                buffer.extend(name.as_bytes());
            }
        }
        Ok(buffer)
    }

    fn deserialize(input: &[u8]) -> IResult<&[u8], Vec<TmapOperation>> {
        many0!(
            input,
            alt!(complete!(parse_header) | complete!(parse_create) | complete!(parse_set) | complete!(parse_remove))
        )
    }
}

/// Helper to iterate over pool objects
#[derive(Debug)]
pub struct Pool {
    pub ctx: rados_list_ctx_t,
}

#[derive(Debug)]
pub struct CephObject {
    pub name: String,
    pub entry_locator: String,
    pub namespace: String,
}

impl Iterator for Pool {
    type Item = CephObject;
    fn next(&mut self) -> Option<CephObject> {
        let mut entry_ptr: *mut *const ::libc::c_char = ptr::null_mut();
        let mut key_ptr: *mut *const ::libc::c_char = ptr::null_mut();
        let mut nspace_ptr: *mut *const ::libc::c_char = ptr::null_mut();

        unsafe {
            let ret_code = rados_nobjects_list_next(self.ctx, &mut entry_ptr, &mut key_ptr, &mut nspace_ptr);
            if ret_code == -ENOENT {
                // We're done
                rados_nobjects_list_close(self.ctx);
                None
            } else if ret_code < 0 {
                // Unknown error
                None
            } else {
                let object_name = CStr::from_ptr(entry_ptr as *const ::libc::c_char);
                let mut object_locator = String::new();
                let mut namespace = String::new();
                if !key_ptr.is_null() {
                    object_locator.push_str(&CStr::from_ptr(key_ptr as *const ::libc::c_char).to_string_lossy());
                }
                if !nspace_ptr.is_null() {
                    namespace.push_str(&CStr::from_ptr(nspace_ptr as *const ::libc::c_char).to_string_lossy());
                }

                return Some(CephObject {
                    name: object_name.to_string_lossy().into_owned(),
                    entry_locator: object_locator,
                    namespace: namespace,
                });
            }
        }
    }
}

/// A helper to create rados read operation
/// An object read operation stores a number of operations which can be
/// executed atomically.
#[derive(Debug)]
pub struct ReadOperation {
    pub object_name: String,
    /// flags are set by calling LIBRADOS_OPERATION_NOFLAG |
    /// LIBRADOS_OPERATION_BALANCE_READS
    /// all the other flags are documented in rados.rs
    pub flags: u32,
    read_op_handle: rados_read_op_t,
}

impl Drop for ReadOperation {
    fn drop(&mut self) {
        unsafe {
            rados_release_read_op(self.read_op_handle);
        }
    }
}

/// A helper to create rados write operation
/// An object write operation stores a number of operations which can be
/// executed atomically.
#[derive(Debug)]
pub struct WriteOperation {
    pub object_name: String,
    /// flags are set by calling LIBRADOS_OPERATION_NOFLAG |
    /// LIBRADOS_OPERATION_ORDER_READS_WRITES
    /// all the other flags are documented in rados.rs
    pub flags: u32,
    pub mtime: time_t,
    write_op_handle: rados_write_op_t,
}

impl Drop for WriteOperation {
    fn drop(&mut self) {
        unsafe {
            rados_release_write_op(self.write_op_handle);
        }
    }
}

/// A rados object extended attribute with name and value.
/// Can be iterated over
#[derive(Debug)]
pub struct XAttr {
    pub name: String,
    pub value: String,
    iter: rados_xattrs_iter_t,
}

/// The version of the librados library.
#[derive(Debug)]
pub struct RadosVersion {
    pub major: i32,
    pub minor: i32,
    pub extra: i32,
}

impl XAttr {
    /// Creates a new XAttr.  Call rados_getxattrs to create the iterator for
    /// this struct
    pub fn new(iter: rados_xattrs_iter_t) -> XAttr {
        XAttr {
            name: String::new(),
            value: String::new(),
            iter: iter,
        }
    }
}

impl Iterator for XAttr {
    type Item = XAttr;

    fn next(&mut self) -> Option<Self::Item> {
        // max xattr name is 255 bytes from what I can find
        let mut name_buffer: Vec<u8> = Vec::with_capacity(255);
        // max xattr is 64Kb from what I can find
        let mut value_buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut val_length: usize = 0;
        unsafe {
            let ret_code = rados_getxattrs_next(
                self.iter,
                name_buffer.as_mut_ptr() as *mut *const c_char,
                value_buffer.as_mut_ptr() as *mut *const c_char,
                &mut val_length,
            );

            if ret_code < 0 {
                // Something failed, however Iterator doesn't return Result so we return None
                None
            }
            // end of iterator reached
            else if val_length == 0 {
                rados_getxattrs_end(self.iter);
                None
            } else {
                Some(XAttr {
                    name: String::from_utf8_lossy(&name_buffer).into_owned(),
                    value: String::from_utf8_lossy(&value_buffer).into_owned(),
                    iter: self.iter,
                })
            }
        }
    }
}

/// Owns a ioctx handle
pub struct IoCtx {
    ioctx: rados_ioctx_t,
}

impl Drop for IoCtx {
    fn drop(&mut self) {
        if !self.ioctx.is_null() {
            unsafe {
                rados_ioctx_destroy(self.ioctx);
            }
        }
    }
}

/// Owns a rados handle
pub struct Rados {
    rados: rados_t,
    phantom: PhantomData<IoCtx>,
}

unsafe impl Sync for Rados{}

impl Drop for Rados {
    fn drop(&mut self) {
        if !self.rados.is_null() {
            unsafe {
                rados_shutdown(self.rados);
            }
        }
    }
}

/// Connect to a Ceph cluster and return a connection handle rados_t
pub fn connect_to_ceph<'a>(user_id: &str, config_file: &str) -> RadosResult<Rados> {
    let connect_id = try!(CString::new(user_id));
    let conf_file = try!(CString::new(config_file));
    unsafe {
        let mut cluster_handle: rados_t = ptr::null_mut();
        let ret_code = rados_create(&mut cluster_handle, connect_id.as_ptr());
        if ret_code < 0 {
            return Err(RadosError::new(try!(get_error(ret_code))));
        }
        let ret_code = rados_conf_read_file(cluster_handle, conf_file.as_ptr());
        if ret_code < 0 {
            return Err(RadosError::new(try!(get_error(ret_code))));
        }
        let ret_code = rados_connect(cluster_handle);
        if ret_code < 0 {
            return Err(RadosError::new(try!(get_error(ret_code))));
        }
        Ok(Rados {
            rados: cluster_handle,
            phantom: PhantomData,
        })
    }
}

impl Rados {
    pub fn inner(&self) -> &rados_t {
        &self.rados
    }

    /// Disconnect from a Ceph cluster and destroy the connection handle rados_t
    /// For clean up, this is only necessary after connect_to_ceph() has
    /// succeeded.
    pub fn disconnect_from_ceph(&self) {
        if self.rados.is_null() {
            // No need to do anything
            return;
        }
        unsafe {
            rados_shutdown(self.rados);
        }
    }

    fn conn_guard(&self) -> RadosResult<()> {
        if self.rados.is_null() {
            return Err(RadosError::new(
                "Rados not connected.  Please initialize cluster".to_string(),
            ));
        }
        Ok(())
    }

    /// Set the value of a configuration option
    pub fn config_set(&self, name: &str, value: &str) -> RadosResult<()> {
        if !self.rados.is_null() {
            return Err(RadosError::new(
                "Rados should not be connected when this function is called".into(),
            ));
        }
        let name_str = try!(CString::new(name));
        let value_str = try!(CString::new(value));
        unsafe {
            let ret_code = rados_conf_set(self.rados, name_str.as_ptr(), value_str.as_ptr());
            if ret_code < 0 {
                return Err(RadosError::new(get_error(ret_code as i32)?));
            }
        }
        Ok(())
    }

    /// Get the value of a configuration option
    pub fn config_get(&self, name: &str) -> RadosResult<String> {
        let name_str = try!(CString::new(name));
        // 5K should be plenty for a config key right?
        let mut buffer: Vec<u8> = Vec::with_capacity(5120);
        unsafe {
            let ret_code = rados_conf_get(
                self.rados,
                name_str.as_ptr(),
                buffer.as_mut_ptr() as *mut c_char,
                buffer.capacity(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(get_error(ret_code as i32)?));
            }
            // Ceph doesn't return how many bytes were written
            buffer.set_len(5120);
            // We need to search for the first NUL byte
            let num_bytes = buffer.iter().position(|x| x == &0u8);
            buffer.set_len(num_bytes.unwrap_or(0));
            Ok(String::from_utf8_lossy(&buffer).into_owned())
        }
    }

    /// Create an io context. The io context allows you to perform operations
    /// within a particular pool.
    /// For more details see rados_ioctx_t.
    pub fn get_rados_ioctx(&self, pool_name: &str) -> RadosResult<IoCtx> {
        self.conn_guard()?;
        let pool_name_str = try!(CString::new(pool_name));
        unsafe {
            let mut ioctx: rados_ioctx_t = ptr::null_mut();
            let ret_code = rados_ioctx_create(self.rados, pool_name_str.as_ptr(), &mut ioctx);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            Ok(IoCtx { ioctx: ioctx })
        }
    }

    /// Create an io context. The io context allows you to perform operations
    /// within a particular pool.
    /// For more details see rados_ioctx_t.
    pub fn get_rados_ioctx2(&self, pool_id: i64) -> RadosResult<IoCtx> {
        self.conn_guard()?;
        unsafe {
            let mut ioctx: rados_ioctx_t = ptr::null_mut();
            let ret_code = rados_ioctx_create2(self.rados, pool_id, &mut ioctx);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            Ok(IoCtx { ioctx: ioctx })
        }
    }
}

impl IoCtx {
    pub fn inner(&self) -> &rados_ioctx_t {
        &self.ioctx
    }

    /// This just tells librados that you no longer need to use the io context.
    /// It may not be freed immediately if there are pending asynchronous
    /// requests on it, but you
    /// should not use an io context again after calling this function on it.
    /// This does not guarantee any asynchronous writes have completed. You must
    /// call rados_aio_flush()
    /// on the io context before destroying it to do that.
    pub fn destroy_rados_ioctx(&self) {
        if self.ioctx.is_null() {
            // No need to do anything
            return;
        }
        unsafe {
            rados_ioctx_destroy(self.ioctx);
        }
    }
    fn ioctx_guard(&self) -> RadosResult<()> {
        if self.ioctx.is_null() {
            return Err(RadosError::new(
                "Rados ioctx not created.  Please initialize first".to_string(),
            ));
        }
        Ok(())
    }
    /// Note: Ceph uses kibibytes: https://en.wikipedia.org/wiki/Kibibyte
    pub fn rados_stat_pool(&self) -> RadosResult<Struct_rados_pool_stat_t> {
        self.ioctx_guard()?;
        let mut pool_stat = Struct_rados_pool_stat_t::default();
        unsafe {
            let ret_code = rados_ioctx_pool_stat(self.ioctx, &mut pool_stat);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            Ok(pool_stat)
        }
    }

    pub fn rados_pool_set_auid(&self, auid: u64) -> RadosResult<()> {
        self.ioctx_guard()?;
        unsafe {
            let ret_code = rados_ioctx_pool_set_auid(self.ioctx, auid);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            return Ok(());
        }
    }

    pub fn rados_pool_get_auid(&self) -> RadosResult<u64> {
        self.ioctx_guard()?;
        let mut auid: u64 = 0;
        unsafe {
            let ret_code = rados_ioctx_pool_get_auid(self.ioctx, &mut auid);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            return Ok(auid);
        }
    }

    /// Test whether the specified pool requires alignment or not.
    pub fn rados_pool_requires_alignment(&self) -> RadosResult<bool> {
        self.ioctx_guard()?;
        unsafe {
            let ret_code = rados_ioctx_pool_requires_alignment(self.ioctx);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            if ret_code == 0 {
                return Ok(false);
            } else {
                return Ok(true);
            }
        }
    }

    /// Get the alignment flavor of a pool
    pub fn rados_pool_required_alignment(&self) -> RadosResult<u64> {
        self.ioctx_guard()?;
        unsafe {
            let ret_code = rados_ioctx_pool_required_alignment(self.ioctx);
            return Ok(ret_code);
        }
    }

    /// Get the pool id of the io context
    pub fn rados_object_get_id(&self) -> RadosResult<i64> {
        self.ioctx_guard()?;
        unsafe {
            let pool_id = rados_ioctx_get_id(self.ioctx);
            Ok(pool_id)
        }
    }

    /// Get the pool name of the io context
    pub fn rados_get_pool_name(&self) -> RadosResult<String> {
        self.ioctx_guard()?;
        let mut buffer: Vec<u8> = Vec::with_capacity(500);

        unsafe {
            // length of string stored, or -ERANGE if buffer too small
            let ret_code = rados_ioctx_get_pool_name(
                self.ioctx,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.capacity() as c_uint,
            );
            if ret_code == -ERANGE {
                // Buffer was too small
                buffer.reserve(1000);
                buffer.set_len(1000);
                let ret_code = rados_ioctx_get_pool_name(
                    self.ioctx,
                    buffer.as_mut_ptr() as *mut c_char,
                    buffer.capacity() as c_uint,
                );
                if ret_code < 0 {
                    return Err(RadosError::new(try!(get_error(ret_code as i32))));
                }
                return Ok(String::from_utf8_lossy(&buffer).into_owned());
            } else if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            } else {
                buffer.set_len(ret_code as usize);
                return Ok(String::from_utf8_lossy(&buffer).into_owned());
            }
        }
    }

    /// Set the key for mapping objects to pgs within an io context.
    pub fn rados_locator_set_key(&self, key: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let key_str = try!(CString::new(key));
        unsafe {
            rados_ioctx_locator_set_key(self.ioctx, key_str.as_ptr());
        }
        Ok(())
    }

    /// Set the namespace for objects within an io context
    /// The namespace specification further refines a pool into different
    /// domains. The mapping of objects to pgs is also based on this value.
    pub fn rados_set_namespace(&self, namespace: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let namespace_str = try!(CString::new(namespace));
        unsafe {
            rados_ioctx_set_namespace(self.ioctx, namespace_str.as_ptr());
        }
        Ok(())
    }

    /// Start listing objects in a pool
    pub fn rados_list_pool_objects(&self) -> RadosResult<rados_list_ctx_t> {
        self.ioctx_guard()?;
        let mut rados_list_ctx: rados_list_ctx_t = ptr::null_mut();
        unsafe {
            let ret_code = rados_nobjects_list_open(self.ioctx, &mut rados_list_ctx);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(rados_list_ctx)
    }

    /// Create a pool-wide snapshot
    pub fn rados_snap_create(&self, snap_name: &str) -> RadosResult<()> {
        self.ioctx_guard()?;

        let snap_name_str = try!(CString::new(snap_name));
        unsafe {
            let ret_code = rados_ioctx_snap_create(self.ioctx, snap_name_str.as_ptr());
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Delete a pool snapshot
    pub fn rados_snap_remove(&self, snap_name: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let snap_name_str = try!(CString::new(snap_name));

        unsafe {
            let ret_code = rados_ioctx_snap_remove(self.ioctx, snap_name_str.as_ptr());
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Rollback an object to a pool snapshot
    /// The contents of the object will be the same as when the snapshot was
    /// taken.
    pub fn rados_snap_rollback(&self, object_name: &str, snap_name: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let snap_name_str = try!(CString::new(snap_name));
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_ioctx_snap_rollback(self.ioctx, object_name_str.as_ptr(), snap_name_str.as_ptr());
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Set the snapshot from which reads are performed.
    /// Subsequent reads will return data as it was at the time of that
    /// snapshot.
    pub fn rados_snap_set_read(&self, snap_id: u64) -> RadosResult<()> {
        self.ioctx_guard()?;

        unsafe {
            rados_ioctx_snap_set_read(self.ioctx, snap_id);
        }
        Ok(())
    }

    /// Allocate an ID for a self-managed snapshot
    /// Get a unique ID to put in the snaphot context to create a snapshot.
    /// A clone of an object is not created until a write with the new snapshot
    /// context is completed.
    pub fn rados_selfmanaged_snap_create(&self) -> RadosResult<u64> {
        self.ioctx_guard()?;
        let mut snap_id: u64 = 0;
        unsafe {
            let ret_code = rados_ioctx_selfmanaged_snap_create(self.ioctx, &mut snap_id);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(snap_id)
    }

    /// Remove a self-managed snapshot
    /// This increases the snapshot sequence number, which will cause snapshots
    /// to be removed lazily.
    pub fn rados_selfmanaged_snap_remove(&self, snap_id: u64) -> RadosResult<()> {
        self.ioctx_guard()?;

        unsafe {
            let ret_code = rados_ioctx_selfmanaged_snap_remove(self.ioctx, snap_id);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Rollback an object to a self-managed snapshot
    /// The contents of the object will be the same as when the snapshot was
    /// taken.
    pub fn rados_selfmanaged_snap_rollback(&self, object_name: &str, snap_id: u64) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_ioctx_selfmanaged_snap_rollback(self.ioctx, object_name_str.as_ptr(), snap_id);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Set the snapshot context for use when writing to objects
    /// This is stored in the io context, and applies to all future writes.
    // pub fn rados_selfmanaged_snap_set_write_ctx(ctx: rados_ioctx_t) ->
    // RadosResult<()> {
    // if ctx.is_null() {
    // return Err(RadosError::new("Rados ioctx not created.  Please initialize
    // first".to_string()));
    // }
    //
    // unsafe {
    // }
    // }
    //
    /// List all the ids of pool snapshots
    // pub fn rados_snap_list(ctx: rados_ioctx_t, snaps: *mut rados_snap_t) ->
    // RadosResult<()> {
    // if ctx.is_null() {
    // return Err(RadosError::new("Rados ioctx not created.  Please initialize
    // first".to_string()));
    // }
    // let mut buffer: Vec<u64> = Vec::with_capacity(500);
    //
    //
    // unsafe {
    // let ret_code = rados_ioctx_snap_list(ctx, &mut buffer, buffer.capacity());
    // if ret_code == -ERANGE {
    // }
    // if ret_code < 0 {
    // return Err(RadosError::new(try!(get_error(ret_code as i32))));
    // }
    // }
    // Ok(buffer)
    // }
    //
    /// Get the id of a pool snapshot
    pub fn rados_snap_lookup(&self, snap_name: &str) -> RadosResult<u64> {
        self.ioctx_guard()?;
        let snap_name_str = try!(CString::new(snap_name));
        let mut snap_id: u64 = 0;
        unsafe {
            let ret_code = rados_ioctx_snap_lookup(self.ioctx, snap_name_str.as_ptr(), &mut snap_id);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(snap_id)
    }

    /// Get the name of a pool snapshot
    pub fn rados_snap_get_name(&self, snap_id: u64) -> RadosResult<String> {
        self.ioctx_guard()?;

        let out_buffer: Vec<u8> = Vec::with_capacity(500);
        let out_buff_size = out_buffer.capacity();
        let out_str = try!(CString::new(out_buffer));
        unsafe {
            let ret_code = rados_ioctx_snap_get_name(
                self.ioctx,
                snap_id,
                out_str.as_ptr() as *mut c_char,
                out_buff_size as c_int,
            );
            if ret_code == -ERANGE {}
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(out_str.to_string_lossy().into_owned())
    }

    /// Find when a pool snapshot occurred
    pub fn rados_snap_get_stamp(&self, snap_id: u64) -> RadosResult<time_t> {
        self.ioctx_guard()?;

        let mut time_id: time_t = 0;
        unsafe {
            let ret_code = rados_ioctx_snap_get_stamp(self.ioctx, snap_id, &mut time_id);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(time_id)
    }

    /// Return the version of the last object read or written to.
    /// This exposes the internal version number of the last object read or
    /// written via this io context
    pub fn rados_get_object_last_version(&self) -> RadosResult<u64> {
        self.ioctx_guard()?;
        unsafe {
            let obj_id = rados_get_last_version(self.ioctx);
            Ok(obj_id)
        }
    }

    /// Write len bytes from buf into the oid object, starting at offset off.
    /// The value of len must be <= UINT_MAX/2.
    pub fn rados_object_write(&self, object_name: &str, buffer: &[u8], offset: u64) -> RadosResult<()> {
        self.ioctx_guard()?;
        let obj_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_write(
                self.ioctx,
                obj_name_str.as_ptr(),
                buffer.as_ptr() as *const c_char,
                buffer.len(),
                offset,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// The object is filled with the provided data. If the object exists, it is
    /// atomically
    /// truncated and then written.
    pub fn rados_object_write_full(&self, object_name: &str, buffer: &[u8]) -> RadosResult<()> {
        self.ioctx_guard()?;
        let obj_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_write_full(
                self.ioctx,
                obj_name_str.as_ptr(),
                buffer.as_ptr() as *const ::libc::c_char,
                buffer.len(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Efficiently copy a portion of one object to another
    /// If the underlying filesystem on the OSD supports it, this will be a
    /// copy-on-write clone.
    /// The src and dest objects must be in the same pg. To ensure this, the io
    /// context should
    /// have a locator key set (see rados_ioctx_locator_set_key()).
    pub fn rados_object_clone_range(
        &self,
        dst_object_name: &str,
        dst_offset: u64,
        src_object_name: &str,
        src_offset: u64,
        length: usize,
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let dst_name_str = try!(CString::new(dst_object_name));
        let src_name_str = try!(CString::new(src_object_name));

        unsafe {
            let ret_code = rados_clone_range(
                self.ioctx,
                dst_name_str.as_ptr(),
                dst_offset,
                src_name_str.as_ptr(),
                src_offset,
                length,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Append len bytes from buf into the oid object.
    pub fn rados_object_append(&self, object_name: &str, buffer: &[u8]) -> RadosResult<()> {
        self.ioctx_guard()?;
        let obj_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_append(
                self.ioctx,
                obj_name_str.as_ptr(),
                buffer.as_ptr() as *const c_char,
                buffer.len(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Read data from an object.  This fills the slice given and returns the
    /// amount of bytes read
    /// The io context determines the snapshot to read from, if any was set by
    /// rados_ioctx_snap_set_read().
    /// Default read size is 64K unless you call Vec::with_capacity(1024*128)
    /// with a larger size.
    pub fn rados_object_read(
        &self,
        object_name: &str,
        fill_buffer: &mut Vec<u8>,
        read_offset: u64,
    ) -> RadosResult<i32> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let mut len = fill_buffer.capacity();
        if len == 0 {
            fill_buffer.reserve_exact(1024 * 64);
            len = fill_buffer.capacity();
        }

        unsafe {
            let ret_code = rados_read(
                self.ioctx,
                object_name_str.as_ptr(),
                fill_buffer.as_mut_ptr() as *mut c_char,
                len,
                read_offset,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
            fill_buffer.set_len(ret_code as usize);
            Ok(ret_code)
        }
    }

    /// Delete an object
    /// Note: This does not delete any snapshots of the object.
    pub fn rados_object_remove(&self, object_name: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_remove(self.ioctx, object_name_str.as_ptr() as *const c_char);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Resize an object
    /// If this enlarges the object, the new area is logically filled with
    /// zeroes. If this shrinks the object, the excess data is removed.
    pub fn rados_object_trunc(&self, object_name: &str, new_size: u64) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_trunc(self.ioctx, object_name_str.as_ptr(), new_size);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Get the value of an extended attribute on an object.
    pub fn rados_object_getxattr(
        &self,
        object_name: &str,
        attr_name: &str,
        fill_buffer: &mut [u8],
    ) -> RadosResult<i32> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let attr_name_str = try!(CString::new(attr_name));

        unsafe {
            let ret_code = rados_getxattr(
                self.ioctx,
                object_name_str.as_ptr() as *const c_char,
                attr_name_str.as_ptr() as *const c_char,
                fill_buffer.as_mut_ptr() as *mut c_char,
                fill_buffer.len(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
            Ok(ret_code)
        }
    }

    /// Set an extended attribute on an object.
    pub fn rados_object_setxattr(&self, object_name: &str, attr_name: &str, attr_value: &mut [u8]) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let attr_name_str = try!(CString::new(attr_name));

        unsafe {
            let ret_code = rados_setxattr(
                self.ioctx,
                object_name_str.as_ptr() as *const c_char,
                attr_name_str.as_ptr() as *const c_char,
                attr_value.as_mut_ptr() as *mut c_char,
                attr_value.len(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Delete an extended attribute from an object.
    pub fn rados_object_rmxattr(&self, object_name: &str, attr_name: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let attr_name_str = try!(CString::new(attr_name));

        unsafe {
            let ret_code = rados_rmxattr(
                self.ioctx,
                object_name_str.as_ptr() as *const c_char,
                attr_name_str.as_ptr() as *const c_char,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Get the rados_xattrs_iter_t reference to iterate over xattrs on an
    /// object Used in conjuction with XAttr::new() to iterate.
    pub fn rados_get_xattr_iterator(&self, object_name: &str) -> RadosResult<rados_xattrs_iter_t> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let mut xattr_iterator_handle: rados_xattrs_iter_t = ptr::null_mut();

        unsafe {
            let ret_code = rados_getxattrs(self.ioctx, object_name_str.as_ptr(), &mut xattr_iterator_handle);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(xattr_iterator_handle)
    }

    /// Get object stats (size,SystemTime)
    pub fn rados_object_stat(&self, object_name: &str) -> RadosResult<(u64, SystemTime)> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let mut psize: u64 = 0;
        let mut time: ::libc::time_t = 0;

        unsafe {
            let ret_code = rados_stat(self.ioctx, object_name_str.as_ptr(), &mut psize, &mut time);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok((psize, (UNIX_EPOCH + Duration::from_secs(time as u64))))
    }

    /// Update tmap (trivial map)
    pub fn rados_object_tmap_update(&self, object_name: &str, update: TmapOperation) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let buffer = try!(update.serialize());
        unsafe {
            let ret_code = rados_tmap_update(
                self.ioctx,
                object_name_str.as_ptr(),
                buffer.as_ptr() as *const c_char,
                buffer.len(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Fetch complete tmap (trivial map) object
    pub fn rados_object_tmap_get(&self, object_name: &str) -> RadosResult<Vec<TmapOperation>> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let mut buffer: Vec<u8> = Vec::with_capacity(500);

        unsafe {
            let ret_code = rados_tmap_get(
                self.ioctx,
                object_name_str.as_ptr(),
                buffer.as_mut_ptr() as *mut c_char,
                buffer.capacity(),
            );
            if ret_code == -ERANGE {
                buffer.reserve(1000);
                buffer.set_len(1000);
                let ret_code = rados_tmap_get(
                    self.ioctx,
                    object_name_str.as_ptr(),
                    buffer.as_mut_ptr() as *mut c_char,
                    buffer.capacity(),
                );
                if ret_code < 0 {
                    return Err(RadosError::new(try!(get_error(ret_code as i32))));
                }
            } else if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        match TmapOperation::deserialize(&buffer) {
            IResult::Done(_, tmap) => Ok(tmap),
            IResult::Incomplete(needed) => Err(RadosError::new(format!(
                "deserialize of ceph tmap failed.
            Input from Ceph was too small.  Needed: {:?} more bytes",
                needed
            ))),
            IResult::Error(e) => Err(RadosError::new(e.to_string())),
        }
    }

    /// Execute an OSD class method on an object
    /// The OSD has a plugin mechanism for performing complicated operations on
    /// an object atomically.
    /// These plugins are called classes. This function allows librados users to
    /// call the custom
    /// methods. The input and output formats are defined by the class. Classes
    /// in ceph.git can
    /// be found in src/cls subdirectories
    pub fn rados_object_exec(
        &self,
        object_name: &str,
        class_name: &str,
        method_name: &str,
        input_buffer: &[u8],
        output_buffer: &mut [u8],
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let class_name_str = try!(CString::new(class_name));
        let method_name_str = try!(CString::new(method_name));

        unsafe {
            let ret_code = rados_exec(
                self.ioctx,
                object_name_str.as_ptr(),
                class_name_str.as_ptr(),
                method_name_str.as_ptr(),
                input_buffer.as_ptr() as *const c_char,
                input_buffer.len(),
                output_buffer.as_mut_ptr() as *mut c_char,
                output_buffer.len(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Sychronously notify watchers of an object
    /// This blocks until all watchers of the object have received and reacted
    /// to the notify, or a timeout is reached.
    pub fn rados_object_notify(&self, object_name: &str, data: &[u8]) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_notify(
                self.ioctx,
                object_name_str.as_ptr(),
                0,
                data.as_ptr() as *const c_char,
                data.len() as i32,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }
    // pub fn rados_object_notify2(ctx: rados_ioctx_t, object_name: &str) ->
    // RadosResult<()> {
    // if ctx.is_null() {
    // return Err(RadosError::new("Rados ioctx not created.  Please initialize
    // first".to_string()));
    // }
    //
    // unsafe {
    // }
    // }
    //
    /// Acknolwedge receipt of a notify
    pub fn rados_object_notify_ack(
        &self,
        object_name: &str,
        notify_id: u64,
        cookie: u64,
        buffer: Option<&[u8]>,
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        match buffer {
            Some(buf) => unsafe {
                let ret_code = rados_notify_ack(
                    self.ioctx,
                    object_name_str.as_ptr(),
                    notify_id,
                    cookie,
                    buf.as_ptr() as *const c_char,
                    buf.len() as i32,
                );
                if ret_code < 0 {
                    return Err(RadosError::new(try!(get_error(ret_code as i32))));
                }
            },
            None => unsafe {
                let ret_code =
                    rados_notify_ack(self.ioctx, object_name_str.as_ptr(), notify_id, cookie, ptr::null(), 0);
                if ret_code < 0 {
                    return Err(RadosError::new(try!(get_error(ret_code as i32))));
                }
            },
        }
        Ok(())
    }
    /// Set allocation hint for an object
    /// This is an advisory operation, it will always succeed (as if it was
    /// submitted with a
    /// LIBRADOS_OP_FLAG_FAILOK flag set) and is not guaranteed to do anything
    /// on the backend.
    pub fn rados_object_set_alloc_hint(
        &self,
        object_name: &str,
        expected_object_size: u64,
        expected_write_size: u64,
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_set_alloc_hint(
                self.ioctx,
                object_name_str.as_ptr(),
                expected_object_size,
                expected_write_size,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    // Perform a compound read operation synchronously
    pub fn rados_perform_read_operations(&self, read_op: ReadOperation) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(read_op.object_name.clone()));

        unsafe {
            let ret_code = rados_read_op_operate(
                read_op.read_op_handle,
                self.ioctx,
                object_name_str.as_ptr(),
                read_op.flags as i32,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    // Perform a compound write operation synchronously
    pub fn rados_commit_write_operations(&self, write_op: &mut WriteOperation) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(write_op.object_name.clone()));

        unsafe {
            let ret_code = rados_write_op_operate(
                write_op.write_op_handle,
                self.ioctx,
                object_name_str.as_ptr(),
                &mut write_op.mtime,
                write_op.flags as i32,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Take an exclusive lock on an object.
    pub fn rados_object_lock_exclusive(
        &self,
        object_name: &str,
        lock_name: &str,
        cookie_name: &str,
        description: &str,
        duration_time: &mut timeval,
        lock_flags: u8,
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let lock_name_str = try!(CString::new(lock_name));
        let cookie_name_str = try!(CString::new(cookie_name));
        let description_str = try!(CString::new(description));

        unsafe {
            let ret_code = rados_lock_exclusive(
                self.ioctx,
                object_name_str.as_ptr(),
                lock_name_str.as_ptr(),
                cookie_name_str.as_ptr(),
                description_str.as_ptr(),
                duration_time,
                lock_flags,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Take a shared lock on an object.
    pub fn rados_object_lock_shared(
        &self,
        object_name: &str,
        lock_name: &str,
        cookie_name: &str,
        description: &str,
        tag_name: &str,
        duration_time: &mut timeval,
        lock_flags: u8,
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let lock_name_str = try!(CString::new(lock_name));
        let cookie_name_str = try!(CString::new(cookie_name));
        let description_str = try!(CString::new(description));
        let tag_name_str = try!(CString::new(tag_name));

        unsafe {
            let ret_code = rados_lock_shared(
                self.ioctx,
                object_name_str.as_ptr(),
                lock_name_str.as_ptr(),
                cookie_name_str.as_ptr(),
                tag_name_str.as_ptr(),
                description_str.as_ptr(),
                duration_time,
                lock_flags,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Release a shared or exclusive lock on an object.
    pub fn rados_object_unlock(&self, object_name: &str, lock_name: &str, cookie_name: &str) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let lock_name_str = try!(CString::new(lock_name));
        let cookie_name_str = try!(CString::new(cookie_name));

        unsafe {
            let ret_code = rados_unlock(
                self.ioctx,
                object_name_str.as_ptr(),
                lock_name_str.as_ptr(),
                cookie_name_str.as_ptr(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// List clients that have locked the named object lock and information
    /// about the lock.
    /// The number of bytes required in each buffer is put in the corresponding
    /// size out parameter.
    /// If any of the provided buffers are too short, -ERANGE is returned after
    /// these sizes are filled in.
    // pub fn rados_object_list_lockers(ctx: rados_ioctx_t, object_name: &str,
    // lock_name: &str, exclusive: u8, ) ->
    // RadosResult<isize> {
    // if ctx.is_null() {
    // return Err(RadosError::new("Rados ioctx not created.  Please initialize
    // first".to_string()));
    // }
    // let object_name_str = try!(CString::new(object_name));
    //
    // unsafe {
    // let ret_code = rados_list_lockers(ctx,
    // o: *const ::libc::c_char,
    // name: *const ::libc::c_char,
    // exclusive: *mut ::libc::c_int,
    // tag: *mut ::libc::c_char,
    // tag_len: *mut size_t,
    // clients: *mut ::libc::c_char,
    // clients_len: *mut size_t,
    // cookies: *mut ::libc::c_char,
    // cookies_len: *mut size_t,
    // addrs: *mut ::libc::c_char,
    // addrs_len: *mut size_t);
    // }
    // }
    /// Releases a shared or exclusive lock on an object, which was taken by the
    /// specified client.
    pub fn rados_object_break_lock(
        &self,
        object_name: &str,
        lock_name: &str,
        client_name: &str,
        cookie_name: &str,
    ) -> RadosResult<()> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let lock_name_str = try!(CString::new(lock_name));
        let cookie_name_str = try!(CString::new(cookie_name));
        let client_name_str = try!(CString::new(client_name));

        unsafe {
            let ret_code = rados_break_lock(
                self.ioctx,
                object_name_str.as_ptr(),
                lock_name_str.as_ptr(),
                client_name_str.as_ptr(),
                cookie_name_str.as_ptr(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }
}

impl Rados {
    pub fn rados_blacklist_client(&self, client: IpAddr, expire_seconds: u32) -> RadosResult<()> {
        self.conn_guard()?;
        let client_address = try!(CString::new(client.to_string()));
        unsafe {
            let ret_code = rados_blacklist_add(self.rados, client_address.as_ptr() as *mut c_char, expire_seconds);

            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
        Ok(())
    }

    /// Returns back a collection of Rados Pools
    ///
    /// pool_buffer should be allocated with:
    /// ```
    /// let capacity = 10;
    /// let pool_buffer: Vec<u8> = Vec::with_capacity(capacity);
    /// ```
    /// buf_size should be the value used with_capacity
    ///
    /// Returns Ok(Vec<String>) - A list of Strings of the pool names.
    ///
    #[allow(unused_variables)]
    pub fn rados_pools(&self) -> RadosResult<Vec<String>> {
        self.conn_guard()?;
        let mut pools: Vec<String> = Vec::new();
        let pool_slice: &[u8];
        let mut pool_buffer: Vec<u8> = Vec::with_capacity(500);

        unsafe {
            let len = rados_pool_list(
                self.rados,
                pool_buffer.as_mut_ptr() as *mut c_char,
                pool_buffer.capacity(),
            );
            if len > pool_buffer.capacity() as i32 {
                // rados_pool_list requires more buffer than we gave it
                pool_buffer.reserve(len as usize);
                let len = rados_pool_list(
                    self.rados,
                    pool_buffer.as_mut_ptr() as *mut c_char,
                    pool_buffer.capacity(),
                );
                // Tell the Vec how much Ceph read into the buffer
                pool_buffer.set_len(len as usize);
            } else {
                // Tell the Vec how much Ceph read into the buffer
                pool_buffer.set_len(len as usize);
            }
        }
        let mut cursor = Cursor::new(&pool_buffer);
        loop {
            let mut string_buf: Vec<u8> = Vec::new();
            let read = try!(cursor.read_until(0x00, &mut string_buf));
            if read == 0 {
                // End of the pool_buffer;
                break;
            } else if read == 1 {
                // Read a double \0.  Time to break
                break;
            } else {
                // Read a String
                pools.push(String::from_utf8_lossy(&string_buf[..read - 1]).into_owned());
            }
        }

        Ok(pools)
    }

    /// Create a pool with default settings
    /// The default owner is the admin user (auid 0). The default crush rule is
    /// rule 0.
    pub fn rados_create_pool(&self, pool_name: &str) -> RadosResult<()> {
        self.conn_guard()?;
        let pool_name_str = try!(CString::new(pool_name));
        unsafe {
            let ret_code = rados_pool_create(self.rados, pool_name_str.as_ptr());
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
        }
        return Ok(());
    }
    /// Delete a pool and all data inside it
    /// The pool is removed from the cluster immediately, but the actual data is
    /// deleted in
    /// the background.
    pub fn rados_delete_pool(&self, pool_name: &str) -> RadosResult<()> {
        self.conn_guard()?;
        let pool_name_str = try!(CString::new(pool_name));
        unsafe {
            let ret_code = rados_pool_delete(self.rados, pool_name_str.as_ptr());
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
        }
        return Ok(());
    }

    /// Lookup a Ceph pool id.  If the pool doesn't exist it will return
    /// Ok(None).
    pub fn rados_lookup_pool(&self, pool_name: &str) -> RadosResult<Option<i64>> {
        self.conn_guard()?;
        let pool_name_str = try!(CString::new(pool_name));
        unsafe {
            let ret_code: i64 = rados_pool_lookup(self.rados, pool_name_str.as_ptr());
            if ret_code >= 0 {
                return Ok(Some(ret_code));
            } else if ret_code as i32 == -ENOENT {
                return Ok(None);
            } else {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
        }
    }

    pub fn rados_reverse_lookup_pool(&self, pool_id: i64) -> RadosResult<String> {
        self.conn_guard()?;
        let mut buffer: Vec<u8> = Vec::with_capacity(500);

        unsafe {
            let ret_code = rados_pool_reverse_lookup(
                self.rados,
                pool_id,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.capacity(),
            );
            if ret_code == -ERANGE {
                // Buffer was too small
                buffer.reserve(1000);
                buffer.set_len(1000);
                let ret_code = rados_pool_reverse_lookup(
                    self.rados,
                    pool_id,
                    buffer.as_mut_ptr() as *mut c_char,
                    buffer.capacity(),
                );
                if ret_code < 0 {
                    return Err(RadosError::new(try!(get_error(ret_code as i32))));
                }
                return Ok(String::from_utf8_lossy(&buffer).into_owned());
            } else if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            } else {
                return Ok(String::from_utf8_lossy(&buffer).into_owned());
            }
        }
    }
}

/// Get the version of librados.
pub fn rados_libversion() -> RadosVersion {
    let mut major: c_int = 0;
    let mut minor: c_int = 0;
    let mut extra: c_int = 0;
    unsafe {
        rados_version(&mut major, &mut minor, &mut extra);
    }
    return RadosVersion {
        major: major,
        minor: minor,
        extra: extra,
    };
}

impl Rados {
    /// Read usage info about the cluster
    /// This tells you total space, space used, space available, and number of
    /// objects.
    /// These are not updated immediately when data is written, they are
    /// eventually consistent.
    /// Note: Ceph uses kibibytes: https://en.wikipedia.org/wiki/Kibibyte
    pub fn rados_stat_cluster(&self) -> RadosResult<Struct_rados_cluster_stat_t> {
        self.conn_guard()?;
        let mut cluster_stat = Struct_rados_cluster_stat_t::default();
        unsafe {
            let ret_code = rados_cluster_stat(self.rados, &mut cluster_stat);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
        }

        return Ok(cluster_stat);
    }

    pub fn rados_fsid(&self) -> RadosResult<Uuid> {
        self.conn_guard()?;
        let mut fsid_buffer: Vec<u8> = Vec::with_capacity(37);
        unsafe {
            let ret_code = rados_cluster_fsid(
                self.rados,
                fsid_buffer.as_mut_ptr() as *mut c_char,
                fsid_buffer.capacity(),
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
            // Tell the Vec how much Ceph read into the buffer
            fsid_buffer.set_len(ret_code as usize);
        }
        // Ceph actually returns the fsid as a uuid string
        let fsid_str = String::from_utf8(fsid_buffer)?;
        // Parse into a UUID and return
        Ok(fsid_str.parse()?)
    }

    /// Ping a monitor to assess liveness
    /// May be used as a simply way to assess liveness, or to obtain
    /// information about the monitor in a simple way even in the
    /// absence of quorum.
    pub fn ping_monitor(&self, mon_id: &str) -> RadosResult<String> {
        self.conn_guard()?;

        let mon_id_str = try!(CString::new(mon_id));
        let out_buffer: Vec<u8> = Vec::with_capacity(500);
        let out_buff_size = out_buffer.capacity();
        let out_str = try!(CString::new(out_buffer));
        unsafe {
            let ret_code = rados_ping_monitor(
                self.rados,
                mon_id_str.as_ptr(),
                out_str.as_ptr() as *mut *mut c_char,
                out_buff_size as *mut usize,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }
        }
        Ok(out_str.to_string_lossy().into_owned())
    }
}

/// Ceph version - Ceph during the make release process generates the version
/// number along with
/// the github hash of the release and embeds the hard coded value into
/// `ceph.py` which is the
/// the default ceph utility.
pub fn ceph_version(socket: &str) -> Option<String> {
    let cmd = "version";

    admin_socket_command(&cmd, socket).ok().and_then(|json| {
        json_data(&json).and_then(|jsondata| json_find(jsondata, &[cmd]).and_then(|data| Some(json_as_string(&data))))
    })
}

/// This version call parses the `ceph -s` output. It does not need `sudo`
/// rights like
/// `ceph_version` does since it pulls from the admin socket.
pub fn ceph_version_parse() -> Option<String> {
    match run_cli("ceph --version") {
        Ok(output) => {
            let n = output.status.code().unwrap();
            if n == 0 {
                Some(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                Some(String::from_utf8_lossy(&output.stderr).to_string())
            }
        }
        Err(_) => None,
    }
}

impl Rados {
    /// Only single String value
    pub fn ceph_status(&self, keys: &[&str]) -> RadosResult<String> {
        self.conn_guard()?;
        match self.ceph_mon_command("prefix", "status", Some("json")) {
            Ok((json, _)) => match json {
                Some(json) => match json_data(&json) {
                    Some(jsondata) => {
                        let data = json_find(jsondata, keys);
                        if data.is_some() {
                            Ok(json_as_string(&data.unwrap()))
                        } else {
                            Err(RadosError::new(
                                "The attributes were not found in the output.".to_string(),
                            ))
                        }
                    }
                    _ => Err(RadosError::new("JSON data not found.".to_string())),
                },
                _ => Err(RadosError::new("JSON data not found.".to_string())),
            },
            Err(e) => Err(e),
        }
    }

    /// string with the `health HEALTH_OK` or `HEALTH_WARN` or `HEALTH_ERR`
    /// which is also not efficient.
    pub fn ceph_health_string(&self) -> RadosResult<String> {
        self.conn_guard()?;
        match self.ceph_mon_command("prefix", "health", None) {
            Ok((data, _)) => Ok(data.unwrap().replace("\n", "")),
            Err(e) => Err(e),
        }
    }

    /// Returns an enum value of:
    /// CephHealth::Ok
    /// CephHealth::Warning
    /// CephHealth::Error
    pub fn ceph_health(&self) -> CephHealth {
        match self.ceph_health_string() {
            Ok(health) => {
                if health.contains("HEALTH_OK") {
                    CephHealth::Ok
                } else if health.contains("HEALTH_WARN") {
                    CephHealth::Warning
                } else {
                    CephHealth::Error
                }
            }
            Err(_) => CephHealth::Error,
        }
    }

    /// Higher level `ceph_command`
    pub fn ceph_command(
        &self,
        name: &str,
        value: &str,
        cmd_type: CephCommandTypes,
        keys: &[&str],
    ) -> RadosResult<JsonData> {
        self.conn_guard()?;
        match cmd_type {
            CephCommandTypes::Osd => Err(RadosError::new("OSD CMDs Not implemented.".to_string())),
            CephCommandTypes::Pgs => Err(RadosError::new("PGS CMDS Not implemented.".to_string())),
            _ => match self.ceph_mon_command(name, value, Some("json")) {
                Ok((json, _)) => match json {
                    Some(json) => match json_data(&json) {
                        Some(jsondata) => {
                            let data = json_find(jsondata, keys);
                            if data.is_some() {
                                Ok(data.unwrap())
                            } else {
                                Err(RadosError::new(
                                    "The attributes were not found in the output.".to_string(),
                                ))
                            }
                        }
                        _ => Err(RadosError::new("JSON data not found.".to_string())),
                    },
                    _ => Err(RadosError::new("JSON data not found.".to_string())),
                },
                Err(e) => Err(e),
            },
        }
    }

    /// Returns the list of available commands
    pub fn ceph_commands(&self, keys: Option<&[&str]>) -> RadosResult<JsonData> {
        self.conn_guard()?;
        match self.ceph_mon_command("prefix", "get_command_descriptions", Some("json")) {
            Ok((json, _)) => match json {
                Some(json) => match json_data(&json) {
                    Some(jsondata) => {
                        if keys.is_some() {
                            let data = json_find(jsondata, keys.unwrap());
                            if data.is_some() {
                                Ok(data.unwrap())
                            } else {
                                Err(RadosError::new(
                                    "The attributes were not found in the output.".to_string(),
                                ))
                            }
                        } else {
                            Ok(jsondata)
                        }
                    }
                    _ => Err(RadosError::new("JSON data not found.".to_string())),
                },
                _ => Err(RadosError::new("JSON data not found.".to_string())),
            },
            Err(e) => Err(e),
        }
    }

    /// Mon command that does not pass in a data payload.
    pub fn ceph_mon_command(
        &self,
        name: &str,
        value: &str,
        format: Option<&str>,
    ) -> RadosResult<(Option<String>, Option<String>)> {
        let data: Vec<*mut c_char> = Vec::with_capacity(1);
        self.ceph_mon_command_with_data(name, value, format, data)
    }

    pub fn ceph_mon_command_without_data(
        &self,
        cmd: &serde_json::Value,
    ) -> RadosResult<Vec<u8>> {
        self.conn_guard()?;
        let cmd_string = cmd.to_string();
        debug!("ceph_mon_command_without_data: {}", cmd_string);
        let data: Vec<*mut c_char> = Vec::with_capacity(1);
        let cmds = CString::new(cmd_string).unwrap();

        let mut outbuf_len = 0;
        let mut outs = ptr::null_mut();
        let mut outs_len = 0;

        // Ceph librados allocates these buffers internally and the pointer that comes
        // back must be
        // freed by call `rados_buffer_free`
        let mut outbuf = ptr::null_mut();
        let mut out: Vec<u8> = vec![];

        debug!("Calling rados_mon_command with {:?}", cmd);

        unsafe {
            // cmd length is 1 because we only allow one command at a time.
            let ret_code = rados_mon_command(
                self.rados,
                &mut cmds.as_ptr(),
                1,
                data.as_ptr() as *mut c_char,
                data.len() as usize,
                &mut outbuf,
                &mut outbuf_len,
                &mut outs,
                &mut outs_len,
            );
            debug!("return code: {}", ret_code);
            if ret_code < 0 {
                if outs_len > 0 && !outs.is_null() {
                    let slice = ::std::slice::from_raw_parts(outs as *const u8, outs_len as usize);
                    rados_buffer_free(outs);
                    return Err(RadosError::new(String::from_utf8_lossy(slice).into_owned()));
                }
                return Err(RadosError::new(get_error(ret_code)?));
            }

            // Copy the data from outbuf and then call rados_buffer_free instead libc::free
            if outbuf_len > 0 && !outbuf.is_null() {
                let slice = ::std::slice::from_raw_parts(outbuf as *const u8, outbuf_len as usize);
                out = slice.to_vec();

                rados_buffer_free(outbuf);
            }
        }

        Ok(out)
    }

    /// Mon command that does pass in a data payload.
    /// Most all of the commands pass through this function.
    pub fn ceph_mon_command_with_data(
        &self,
        name: &str,
        value: &str,
        format: Option<&str>,
        data: Vec<*mut c_char>,
    ) -> RadosResult<(Option<String>, Option<String>)> {
        self.conn_guard()?;

        let mut cmd_strings: Vec<String> = Vec::new();
        match format {
            Some(fmt) => cmd_strings.push(format!("{{\"{}\": \"{}\", \"format\": \"{}\"}}", name, value, fmt)),
            None => cmd_strings.push(format!("{{\"{}\": \"{}\"}}", name, value)),
        }

        let cstrings: Vec<CString> = cmd_strings[..]
            .iter()
            .map(|s| CString::new(s.clone()).unwrap())
            .collect();
        let mut cmds: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();

        let mut outbuf = ptr::null_mut();
        let mut outs = ptr::null_mut();
        let mut outbuf_len = 0;
        let mut outs_len = 0;

        // Ceph librados allocates these buffers internally and the pointer that comes
        // back must be
        // freed by call `rados_buffer_free`
        let mut str_outbuf: Option<String> = None;
        let mut str_outs: Option<String> = None;

        debug!("Calling rados_mon_command with {:?}", cstrings);

        unsafe {
            // cmd length is 1 because we only allow one command at a time.
            let ret_code = rados_mon_command(
                self.rados,
                cmds.as_mut_ptr(),
                1,
                data.as_ptr() as *mut c_char,
                data.len() as usize,
                &mut outbuf,
                &mut outbuf_len,
                &mut outs,
                &mut outs_len,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }

            // Copy the data from outbuf and then  call rados_buffer_free instead libc::free
            if outbuf_len > 0 {
                let c_str_outbuf: &CStr = CStr::from_ptr(outbuf);
                let buf_outbuf: &[u8] = c_str_outbuf.to_bytes();
                let str_slice_outbuf: &str = str::from_utf8(buf_outbuf).unwrap();
                str_outbuf = Some(str_slice_outbuf.to_owned());

                rados_buffer_free(outbuf);
            }

            if outs_len > 0 {
                let c_str_outs: &CStr = CStr::from_ptr(outs);
                let buf_outs: &[u8] = c_str_outs.to_bytes();
                let str_slice_outs: &str = str::from_utf8(buf_outs).unwrap();
                str_outs = Some(str_slice_outs.to_owned());

                rados_buffer_free(outs);
            }
        }

        Ok((str_outbuf, str_outs))
    }

    /// OSD command that does not pass in a data payload.
    pub fn ceph_osd_command(
        &self,
        id: i32,
        name: &str,
        value: &str,
        format: Option<&str>,
    ) -> RadosResult<(Option<String>, Option<String>)> {
        let data: Vec<*mut c_char> = Vec::with_capacity(1);
        self.ceph_osd_command_with_data(id, name, value, format, data)
    }

    /// OSD command that does pass in a data payload.
    pub fn ceph_osd_command_with_data(
        &self,
        id: i32,
        name: &str,
        value: &str,
        format: Option<&str>,
        data: Vec<*mut c_char>,
    ) -> RadosResult<(Option<String>, Option<String>)> {
        self.conn_guard()?;

        let mut cmd_strings: Vec<String> = Vec::new();
        match format {
            Some(fmt) => cmd_strings.push(format!("{{\"{}\": \"{}\", \"format\": \"{}\"}}", name, value, fmt)),
            None => cmd_strings.push(format!("{{\"{}\": \"{}\"}}", name, value)),
        }

        let cstrings: Vec<CString> = cmd_strings[..]
            .iter()
            .map(|s| CString::new(s.clone()).unwrap())
            .collect();
        let mut cmds: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();

        let mut outbuf = ptr::null_mut();
        let mut outs = ptr::null_mut();
        let mut outbuf_len = 0;
        let mut outs_len = 0;

        // Ceph librados allocates these buffers internally and the pointer that comes
        // back must be
        // freed by call `rados_buffer_free`
        let mut str_outbuf: Option<String> = None;
        let mut str_outs: Option<String> = None;

        unsafe {
            // cmd length is 1 because we only allow one command at a time.
            let ret_code = rados_osd_command(
                self.rados,
                id,
                cmds.as_mut_ptr(),
                1,
                data.as_ptr() as *mut c_char,
                data.len() as usize,
                &mut outbuf,
                &mut outbuf_len,
                &mut outs,
                &mut outs_len,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }

            // Copy the data from outbuf and then  call rados_buffer_free instead libc::free
            if outbuf_len > 0 {
                let c_str_outbuf: &CStr = CStr::from_ptr(outbuf);
                let buf_outbuf: &[u8] = c_str_outbuf.to_bytes();
                let str_slice_outbuf: &str = str::from_utf8(buf_outbuf).unwrap();
                str_outbuf = Some(str_slice_outbuf.to_owned());

                rados_buffer_free(outbuf);
            }

            if outs_len > 0 {
                let c_str_outs: &CStr = CStr::from_ptr(outs);
                let buf_outs: &[u8] = c_str_outs.to_bytes();
                let str_slice_outs: &str = str::from_utf8(buf_outs).unwrap();
                str_outs = Some(str_slice_outs.to_owned());

                rados_buffer_free(outs);
            }
        }

        Ok((str_outbuf, str_outs))
    }

    /// PG command that does not pass in a data payload.
    pub fn ceph_pgs_command(
        &self,
        pg: &str,
        name: &str,
        value: &str,
        format: Option<&str>,
    ) -> RadosResult<(Option<String>, Option<String>)> {
        let data: Vec<*mut c_char> = Vec::with_capacity(1);
        self.ceph_pgs_command_with_data(pg, name, value, format, data)
    }

    /// PG command that does pass in a data payload.
    pub fn ceph_pgs_command_with_data(
        &self,
        pg: &str,
        name: &str,
        value: &str,
        format: Option<&str>,
        data: Vec<*mut c_char>,
    ) -> RadosResult<(Option<String>, Option<String>)> {
        self.conn_guard()?;

        let mut cmd_strings: Vec<String> = Vec::new();
        match format {
            Some(fmt) => cmd_strings.push(format!("{{\"{}\": \"{}\", \"format\": \"{}\"}}", name, value, fmt)),
            None => cmd_strings.push(format!("{{\"{}\": \"{}\"}}", name, value)),
        }

        let pg_str = CString::new(pg).unwrap();
        let cstrings: Vec<CString> = cmd_strings[..]
            .iter()
            .map(|s| CString::new(s.clone()).unwrap())
            .collect();
        let mut cmds: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();

        let mut outbuf = ptr::null_mut();
        let mut outs = ptr::null_mut();
        let mut outbuf_len = 0;
        let mut outs_len = 0;

        // Ceph librados allocates these buffers internally and the pointer that comes
        // back must be
        // freed by call `rados_buffer_free`
        let mut str_outbuf: Option<String> = None;
        let mut str_outs: Option<String> = None;

        unsafe {
            // cmd length is 1 because we only allow one command at a time.
            let ret_code = rados_pg_command(
                self.rados,
                pg_str.as_ptr(),
                cmds.as_mut_ptr(),
                1,
                data.as_ptr() as *mut c_char,
                data.len() as usize,
                &mut outbuf,
                &mut outbuf_len,
                &mut outs,
                &mut outs_len,
            );
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }

            // Copy the data from outbuf and then  call rados_buffer_free instead libc::free
            if outbuf_len > 0 {
                let c_str_outbuf: &CStr = CStr::from_ptr(outbuf);
                let buf_outbuf: &[u8] = c_str_outbuf.to_bytes();
                let str_slice_outbuf: &str = str::from_utf8(buf_outbuf).unwrap();
                str_outbuf = Some(str_slice_outbuf.to_owned());

                rados_buffer_free(outbuf);
            }

            if outs_len > 0 {
                let c_str_outs: &CStr = CStr::from_ptr(outs);
                let buf_outs: &[u8] = c_str_outs.to_bytes();
                let str_slice_outs: &str = str::from_utf8(buf_outs).unwrap();
                str_outs = Some(str_slice_outs.to_owned());

                rados_buffer_free(outs);
            }
        }

        Ok((str_outbuf, str_outs))
    }
}

/// Owns a rados handle
pub struct Completion {
    completion: rados_completion_t,
}

impl Completion {
    pub fn is_complete(&self) -> RadosResult<bool> {
        unsafe {
            let ret_code = rados_aio_is_complete(self.completion);

            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code))));
            }

            if ret_code == 0 {
                return Ok(false);
            } else {
                return Ok(true);
            }
        }
    }

    pub fn get_return_value(&self) -> i32 {
        unsafe { rados_aio_get_return_value(self.completion) }
    }
}

impl Rados {
    pub fn get_rados_completion(&self) -> RadosResult<Completion> {
        let cb_arg: *mut ::std::os::raw::c_void = ptr::null_mut();

        unsafe {
            let mut completion: rados_completion_t = ptr::null_mut();
            rados_aio_create_completion(cb_arg, None, None, &mut completion);
            Ok(Completion { completion: completion })
        }
    }
}

impl IoCtx {
    pub fn rados_object_aio_read(
        &self,
        object_name: &str,
        completion: &mut Completion,
        fill_buffer: &mut Vec<u8>,
        read_offset: u64,
    ) -> RadosResult<i32> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));
        let mut len = fill_buffer.capacity();
        println!("Buffer capacity: {}", len);
        if len == 0 {
            fill_buffer.reserve_exact(1024 * 64);
            len = fill_buffer.capacity();
            println!("New buffer capacity: {}", len);
        }

        let mut buffer: Vec<u8> = Vec::with_capacity(1000);
        //let mut buffer: Vec<u8> = vec![0u8; 1000];
        //let mut buffer2: [u8; 1000] = [0; 1000];
        unsafe {
            //let mut buf = vec![0u8; 1000];
            //buf = buf.iter().take_while(|&x| x != &0u8).cloned().collect();
            //Ok(String::from_utf8_lossy(&buf).into_owned())

            //let mut a: [c_char; 1000] = [0; 1000];
            let ret_code = rados_aio_read(
                self.ioctx,
                object_name_str.as_ptr(),
                completion.completion,
                //fill_buffer.as_mut_ptr() as *mut c_char,
                //&a[0] as *mut c_char,
                //&mut a[0] as *const c_char,
                buffer.as_mut_ptr() as *mut c_char,
                //buffer2[0] as *mut c_char,
                //len as size_t,
                //10 as size_t,
                1000 as size_t,
                read_offset,
            );
            while !completion.is_complete().unwrap() {}
            //println!("buffer: {:?}", buffer);
            //println!("a: {}{}{}{}{}", a[0], a[1], a[2], a[3], a[4]);
            //buf = buf.iter().take_while(|&x| x != &0u8).cloned().collect();
            buffer.set_len(1000);
            println!("buffer: {:?}", buffer);
            //let a = String::from_utf8_lossy(&buffer).into_owned();
            //println!("String from buf: {:?}", a);
            println!("rados_aio_read returned {}", ret_code);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
            Ok(ret_code)
        }
    }

    /// Get object stats (size,SystemTime)
    pub fn rados_object_aio_stat(
        &self,
        object_name: &str,
        completion: &mut Completion,
        psize: &mut u64,
        time: &mut i64,
    ) -> RadosResult<i32> {
        self.ioctx_guard()?;
        let object_name_str = try!(CString::new(object_name));

        unsafe {
            let ret_code = rados_aio_stat(self.ioctx, object_name_str.as_ptr(), completion.completion, psize, time as *mut ::libc::time_t);
            if ret_code < 0 {
                return Err(RadosError::new(try!(get_error(ret_code as i32))));
            }
            Ok(ret_code)
        }
    }
}
