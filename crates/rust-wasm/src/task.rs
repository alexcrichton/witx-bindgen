#![allow(unused_imports)]

use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

mod export;
mod import;
pub use export::*;
pub use import::*;

// pub mod sample {
//     use super::WriteStream;

//     pub mod generated_abi_glue_for_exports {
//         use super::super::{TaskAbi, WriteStream};

//         #[no_mangle]
//         pub unsafe extern "C" fn the_export(a: usize, b: usize) -> usize {
//             let (a, b) = TaskAbi::run(async move {
//                 // deserialize canonical abi
//                 let arg = String::from_utf8_unchecked(Vec::from_raw_parts(a as *mut u8, b, b));

//                 // call the actual user-defined function
//                 let ret = super::the_export(arg).await;

//                 // serialize into the canonical abi
//                 let ret = ret.into_bytes().into_boxed_slice();
//                 let ret_0 = ret.as_ptr() as u64;
//                 let ret_1 = ret.len() as u64;
//                 std::mem::forget(ret);

//                 // communicate return value
//                 let mut ret_area = [0; 2];
//                 ret_area[0] = ret_0;
//                 ret_area[1] = ret_1;
//                 TaskAbi::await_return(ret_area.as_ptr() as usize).await;
//             });

//             // "return multiple values" through memory
//             RET_AREA[0] = a as u64;
//             RET_AREA[1] = b as u64;
//             RET_AREA.as_ptr() as usize
//         }

//         #[no_mangle]
//         pub unsafe extern "C" fn the_export_on_event(a: usize, b: u32, c: u32) -> u32 {
//             TaskAbi::on_event_abi(a, b, c)
//         }

//         #[no_mangle]
//         pub unsafe extern "C" fn the_export_post_return(a: usize) -> u32 {
//             TaskAbi::post_return_abi(a)
//         }

//         #[no_mangle]
//         pub unsafe extern "C" fn the_export_cancel(a: usize) -> u32 {
//             TaskAbi::cancel_abi(a)
//         }

//         #[no_mangle]
//         pub unsafe extern "C" fn stream_export(a: usize, b: usize) -> usize {
//             let (a, b) = TaskAbi::run(async move {
//                 // deserialize canonical abi
//                 let arg = String::from_utf8_unchecked(Vec::from_raw_parts(a as *mut u8, b, b));

//                 let mut stream = WriteStream::new();

//                 // call the actual user-defined function
//                 let ret = super::stream_export(arg, &mut stream).await;

//                 // serialize into the canonical abi
//                 let ret = ret.into_bytes().into_boxed_slice();
//                 let ret_0 = ret.as_ptr() as u64;
//                 let ret_1 = ret.len() as u64;
//                 std::mem::forget(ret);

//                 // communicate return value
//                 let mut ret_area = [0; 2];
//                 ret_area[0] = ret_0;
//                 ret_area[1] = ret_1;
//                 TaskAbi::await_return(ret_area.as_ptr() as usize).await;
//             });

//             // "return multiple values" through memory
//             RET_AREA[0] = a as u64;
//             RET_AREA[1] = b as u64;
//             RET_AREA.as_ptr() as usize
//         }

//         #[no_mangle]
//         pub unsafe extern "C" fn stream_export_post_write(a: usize, b: usize) -> u32 {
//             TaskAbi::post_write_abi(a, b)
//         }

//         static mut RET_AREA: [u64; 8] = [0; 8];
//     }

//     pub async fn the_export(a: String) -> String {
//         let ret = imports::some_import(&a).await;

//         let mut some_stream = imports::stream_import(&a);

//         let mut buf: Vec<u32> = Vec::new();
//         let mut read1 = some_stream.with_buffer(&mut buf);
//         let amt = read1.read().await + read1.read().await;
//         drop(read1);
//         println!("read {} items", amt);

//         let mut read2 = some_stream.with_buffer(&mut buf);
//         let amt = read2.read().await + read2.read().await;
//         drop(read2);
//         println!("read {} items afterwards", amt);

//         return ret;
//     }

//     pub async fn stream_export(a: String, stream: &mut WriteStream<u8>) -> String {
//         let ret = imports::some_import(&a).await;

//         stream.write(&[]).await;
//         stream.with_buffer(&[]).write().await;

//         return ret;
//     }

//     pub mod imports {
//         use super::super::{Stream, WasmImport};

//         mod abi {
//             #[link(wasm_import_module = "the_module_to_import_from")]
//             extern "C" {
//                 pub fn some_import(a: usize, b: usize, ret: usize) -> u32;
//                 pub fn stream_import(
//                     str_ptr: usize,
//                     str_len: usize,
//                     buf: usize,
//                     buf_len: usize,
//                     ret: usize,
//                 ) -> u32;
//             }
//         }

//         pub fn some_import(a: &str) -> WasmImport<String, 2> {
//             unsafe {
//                 let mut ret = [0usize; 2];
//                 let ret_ptr = ret.as_mut_ptr() as usize;
//                 let task = abi::some_import(a.as_ptr() as usize, a.len(), ret_ptr);
//                 WasmImport::new(task, ret, |ret| {
//                     let ptr = ret[0] as *mut u8;
//                     let len = ret[1] as usize;
//                     String::from_utf8_unchecked(Vec::from_raw_parts(ptr, len, len))
//                 })
//             }
//         }

//         pub fn stream_import(a: &str) -> Stream<u32, (), 0> {
//             unsafe {
//                 let task = abi::stream_import(a.as_ptr() as usize, a.len(), 0, 0, 0);
//                 let import = WasmImport::new(task, [], |_| {});
//                 Stream::new(import)
//             }
//         }
//     }
// }
