use crate::testkit::*;
use crate::{Error, Store};
use weirdnet_update::Hash32;

fn store() -> (tempfile::TempDir, Store) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path(), "weirdbrains").unwrap();
    (d, s)
}

#[test]
fn home_belongs_to_one_org() {
    let (d, s) = store();
    drop(s);
    assert!(matches!(Store::open(d.path(), "other"), Err(Error::Conflict(_))));
    Store::open(d.path(), "weirdbrains").unwrap();
}

#[test]
fn root_chain_needs_old_and_new_keys() {
    let (_d, s) = store();
    let (k1, k2, k3) = (key(1), key(2), key(3));
    let r1 = root("weirdbrains", 1, &[&k1], &k2);
    assert_eq!(s.publish_root(&signed_root(&r1, &[&k1]), NOW).unwrap(), 1);
    // v2 rotates to k3: k3 alone is refused, k1 + k3 is accepted.
    let r2 = root("weirdbrains", 2, &[&k3], &k2);
    assert!(s.publish_root(&signed_root(&r2, &[&k3]), NOW).is_err());
    assert_eq!(s.publish_root(&signed_root(&r2, &[&k1, &k3]), NOW).unwrap(), 2);
    assert_eq!(s.roots().unwrap().len(), 2);
    // Another org's root is refused even when self-consistent.
    let (_d2, s2) = store();
    let other = root("someone-else", 1, &[&k1], &k2);
    assert!(matches!(s2.publish_root(&signed_root(&other, &[&k1]), NOW), Err(Error::Invalid(_))));
}

#[test]
fn manifest_needs_release_key_blobs_and_rising_seq() {
    let (_d, s) = store();
    let (k1, k2) = (key(1), key(2));
    let data: &[u8] = b"app build";
    let m1 = manifest("morph", "stable", 1, "1.0.0", &[("morph.tar.gz", data)]);
    assert!(matches!(s.publish_manifest(&signed_manifest(&m1, &k2), NOW), Err(Error::Invalid(_))), "no root yet");
    s.publish_root(&signed_root(&root("weirdbrains", 1, &[&k1], &k2), &[&k1]), NOW).unwrap();
    assert!(s.publish_manifest(&signed_manifest(&m1, &k2), NOW).is_err(), "blob missing");
    assert!(s.put_blob(&Hash32::of(b"other"), data).is_err(), "hash checked");
    s.put_blob(&Hash32::of(data), data).unwrap();
    assert!(s.publish_manifest(&signed_manifest(&m1, &k1), NOW).is_err(), "root key is not a release key");
    let p = s.publish_manifest(&signed_manifest(&m1, &k2), NOW).unwrap();
    assert_eq!((p.seq, p.update_id.as_str()), (1, "morph-stable-1"));
    assert!(matches!(s.publish_manifest(&signed_manifest(&m1, &k2), NOW), Err(Error::Conflict(_))));
    let m2 = manifest("morph", "stable", 2, "1.1.0", &[("morph.tar.gz", data)]);
    s.publish_manifest(&signed_manifest(&m2, &k2), NOW).unwrap();
    let b = s.bundle("morph", "stable").unwrap();
    assert_eq!(b.roots.len(), 1);
    assert_eq!(b.manifest.unwrap().decode::<weirdnet_update::UpdateManifest>().unwrap().seq, 2);
    assert!(s.bundle("morph", "beta").unwrap().manifest.is_none());
    let mut expired = manifest("morph", "stable", 3, "1.2.0", &[("morph.tar.gz", data)]);
    expired.expires = NOW;
    assert!(s.publish_manifest(&signed_manifest(&expired, &k2), NOW).is_err());
}

#[test]
fn wire_round_trip() {
    let k1 = key(1);
    let env = signed_root(&root("weirdbrains", 1, &[&k1], &key(2)), &[&k1]);
    let b = weirdnet_update::UpdateBundle { roots: vec![env], manifest: None, freshness: None };
    let w = crate::wire::BundleWire::from_bundle(&b).unwrap();
    let json = serde_json::to_string(&w).unwrap();
    let back: crate::wire::BundleWire = serde_json::from_str(&json).unwrap();
    assert_eq!(back.to_bundle().unwrap().roots, b.roots);
}
