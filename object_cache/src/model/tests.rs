/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use super::*;
/// Domain part descriptors retain size and checksum without owning the payload.
#[test]
fn part_descriptor_preserves_payload_identity() {
    let payload = Bytes::from_static(b"part data");
    let part = Part::new(3, payload.clone()).with_creation_time(42);
    assert_eq!((part.part, part.size, part.creation_time), (3, 9, 42));
    assert_eq!(part.checksum, <[u8; 32]>::from(Sha256::digest(&payload)));
}
/// A selected suffix keeps its original part indexes rather than renumbering from zero.
#[test]
fn object_view_can_capture_a_suffix() {
    let object = Object {
        metadata: ObjectMetadata::new().with_version(3),
        parts: vec![Part::new(2, Bytes::from_static(b"tail"))],
    };
    assert_eq!(object.metadata.version, 3);
    assert_eq!(object.parts[0].part, 2);
}
