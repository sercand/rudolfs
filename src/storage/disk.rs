// Copyright (c) 2019 Jason White
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use bytes::{BufMut, Bytes, BytesMut};
use futures::{
    future::{self, Either},
    Future, Stream,
};
use tokio::{
    self,
    codec::{Decoder, Encoder, Framed},
    fs,
};
use uuid::Uuid;

use super::{
    LFSObject, Namespace, Storage, StorageFuture, StorageKey, StorageStream,
};
use crate::lfs::Oid;
use crate::util::NamedTempFile;

pub struct Backend {
    root: PathBuf,
}

impl Backend {
    pub fn new(root: PathBuf) -> impl Future<Item = Self, Error = io::Error> {
        // TODO: Clean out files in the "incomplete" folder.
        future::ok(Backend { root })
    }

    // Use sub directories in order to better utilize the file system's internal
    // tree data structure.
    fn key_to_path(&self, key: &StorageKey) -> PathBuf {
        self.root.join(format!(
            "objects/{}/{}",
            key.namespace(),
            key.oid().path()
        ))
    }
}

impl Storage for Backend {
    type Error = io::Error;

    fn get(
        &self,
        key: &StorageKey,
    ) -> StorageFuture<Option<LFSObject>, Self::Error> {
        Box::new(
            fs::File::open(self.key_to_path(key))
                .and_then(fs::File::metadata)
                .then(move |result| {
                    Ok(match result {
                        Ok((file, metadata)) => {
                            let stream = Framed::new(file, BytesCodec::new())
                                .map(BytesMut::freeze);

                            Some(LFSObject::new(
                                metadata.len(),
                                Box::new(stream),
                            ))
                        }
                        Err(err) => match err.kind() {
                            io::ErrorKind::NotFound => None,
                            _ => return Err(err),
                        },
                    })
                }),
        )
    }

    fn put(
        &self,
        key: StorageKey,
        value: LFSObject,
    ) -> StorageFuture<(), Self::Error> {
        let path = self.key_to_path(&key);
        let dir = path.parent().unwrap().to_path_buf();

        let (len, stream) = value.into_parts();

        let incomplete = self.root.join("incomplete");
        let temp_path = incomplete.join(Uuid::new_v4().to_string());

        Box::new(
            fs::create_dir_all(incomplete)
                .and_then(move |()| {
                    // Note that when this is dropped, the file is deleted.
                    // Thus, if anything goes wrong we are not left with
                    // a temporary file laying around.
                    NamedTempFile::new(temp_path)
                })
                .and_then(move |file| {
                    stream.forward(Framed::new(file, BytesCodec::new()))
                })
                .and_then(move |(_, sink)| {
                    let written = sink.codec().written();
                    let file = sink.into_inner();

                    if written != len {
                        // If we didn't get a full object, we cannot save it to
                        // disk. This can happen if we're using the disk as
                        // a cache and there is an error in the middle of the
                        // upload.
                        Either::A(future::err(io::Error::new(
                            io::ErrorKind::Other,
                            "got incomplete object",
                        )))
                    } else {
                        Either::B(
                            fs::create_dir_all(dir)
                                .and_then(move |()| file.persist(path))
                                .map(|_| ()),
                        )
                    }
                }),
        )
    }

    fn size(
        &self,
        key: &StorageKey,
    ) -> StorageFuture<Option<u64>, Self::Error> {
        let path = self.key_to_path(key);

        Box::new(
            fs::metadata(path)
                .map(move |metadata| Some(metadata.len()))
                .or_else(move |err| match err.kind() {
                    io::ErrorKind::NotFound => Ok(None),
                    _ => Err(err),
                }),
        )
    }

    fn delete(&self, key: &StorageKey) -> StorageFuture<(), Self::Error> {
        // TODO: Attempt to delete the parent folder(s)? This would keep the
        // directory tree clean but it could also cause a race condition when
        // directories are created during `put` operations.
        Box::new(fs::remove_file(self.key_to_path(key)).or_else(move |err| {
            match err.kind() {
                io::ErrorKind::NotFound => Ok(()),
                _ => Err(err),
            }
        }))
    }

    /// Lists the objects that are on disk.
    ///
    /// The directory structure is assumed to be like this:
    ///
    ///     objects/{org}/{project}/
    ///     ├── 00
    ///     │   ├── 07
    ///     │   │   └── 0007941906960...
    ///     │   └── ff
    ///     │       └── 00ff9e9c69224...
    ///     ├── 01
    ///     │   ├── 89
    ///     │   │   └── 0189e5fd19477...
    ///     │   └── f5
    ///     │       └── 01f5c45c65e62...
    ///                 ^^^^
    ///
    /// Note that the first four characters are repeated in the file name so
    /// that transforming the file name into an object ID is simpler.
    fn list(&self) -> StorageStream<(StorageKey, u64), Self::Error> {
        let path = self.root.join("objects");

        Box::new(
            fs::read_dir(path)
                .flatten_stream()
                .map(move |entry| fs::read_dir(entry.path()).flatten_stream())
                .flatten()
                .map(move |entry| fs::read_dir(entry.path()).flatten_stream())
                .flatten()
                .map(move |entry| fs::read_dir(entry.path()).flatten_stream())
                .flatten()
                .map(move |entry| fs::read_dir(entry.path()).flatten_stream())
                .flatten()
                .and_then(move |entry| {
                    let path = entry.path();
                    future::poll_fn(move || entry.poll_metadata())
                        .map(move |metadata| (path, metadata))
                })
                .filter_map(move |(path, metadata)| {
                    // Extract the org and project names from the top two path
                    // components.
                    let project_path = path.parent()?.parent()?.parent()?;

                    let project = project_path.file_name()?.to_str()?;
                    let org = project_path.parent()?.file_name()?.to_str()?;

                    let namespace = Namespace::new(org.into(), project.into());

                    let oid = path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .and_then(|s| Oid::from_str(s).ok())?;

                    if metadata.is_file() {
                        Some((StorageKey::new(namespace, oid), metadata.len()))
                    } else {
                        None
                    }
                }),
        )
    }
}

/// A simple bytes codec that keeps track of its length.
struct BytesCodec {
    written: u64,
}

impl BytesCodec {
    pub fn new() -> Self {
        BytesCodec { written: 0 }
    }

    pub fn written(&self) -> u64 {
        self.written
    }
}

impl Decoder for BytesCodec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(
        &mut self,
        buf: &mut BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        if !buf.is_empty() {
            let len = buf.len();
            Ok(Some(buf.split_to(len)))
        } else {
            Ok(None)
        }
    }
}

impl Encoder for BytesCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(
        &mut self,
        data: Bytes,
        buf: &mut BytesMut,
    ) -> Result<(), io::Error> {
        let len = data.len();
        self.written += len as u64;
        buf.reserve(len);
        buf.put(data);
        Ok(())
    }
}
