// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! S1 of `docs/vault/vault_build_plan.md`: every invariant the vault's
//! crypto core pins. Fixtures are invented seats `a`..`e`.

use ed25519_dalek::SigningKey;
use molt_core::vault::{
    grant_id, payload_aad, resp_aad, secret_id, share_aad, SecretText, VaultBase, VaultBaseDeposit,
    VaultBaseGrant, VaultCtx, VaultDeposit, VaultGrant, VaultRevealOutcome, VAULT_PAYLOAD_MAX,
};
use molt_vault::rand_core::SeedableRng;
use molt_vault::*;
use zeroize::Zeroizing;

const RID: &str = "r1";

struct Seat {
    name: String,
    sk: SigningKey,
    seed: Zeroizing<[u8; 32]>,
    vault_sk: VaultSecretKey,
    vault_pk: String,
}

fn seats(n: u8) -> Vec<Seat> {
    (0..n)
        .map(|i| {
            let name = char::from(b'a' + i).to_string();
            let sk = SigningKey::from_bytes(&[i + 1; 32]);
            let identity_pk = hex::encode(sk.verifying_key().to_bytes());
            let seed = derive_vault_seed(&[0x10 + i; 32], &format!("npk-{name}"), &identity_pk);
            let (vault_sk, vault_pk) = vault_keypair(&seed);
            Seat {
                name,
                sk,
                seed,
                vault_sk,
                vault_pk,
            }
        })
        .collect()
}

fn ctx(seats: &[Seat], m: u8) -> VaultCtx {
    VaultCtx {
        m,
        holders_in_genesis_order: seats
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    hex::encode(s.sk.verifying_key().to_bytes()),
                    s.vault_pk.clone(),
                )
            })
            .collect(),
    }
}

fn rng(seed: u8) -> rand_chacha::ChaCha20Rng {
    rand_chacha::ChaCha20Rng::from_seed([seed; 32])
}

fn text(s: &str) -> SecretText {
    SecretText(s.to_string())
}

fn deposit_by(
    seats: &[Seat],
    ctx: &VaultCtx,
    depositor: usize,
    body: &SecretText,
    r: &mut rand_chacha::ChaCha20Rng,
) -> (VaultDeposit, Vec<u8>) {
    let d = &seats[depositor];
    build_deposit(
        &DepositInput {
            republic_id: RID,
            depositor: &d.name,
            name: "one",
            kind: "text",
            text: body,
            ctx,
        },
        &d.seed,
        &d.sk,
        r,
    )
    .expect("deposit builds")
}

fn x_of(ctx: &VaultCtx, name: &str) -> u8 {
    let i = ctx
        .holders_in_genesis_order
        .iter()
        .position(|(n, _, _)| n == name)
        .expect("a seat");
    u8::try_from(i + 1).expect("small")
}

fn holder_shares(seats: &[Seat], ctx: &VaultCtx, dep: &VaultDeposit) -> Vec<SeatShare> {
    dep.holders
        .iter()
        .map(|h| {
            let seat = seats.iter().find(|s| s.name == *h).expect("a seat");
            let x = x_of(ctx, h);
            let share =
                check_my_share(dep, RID, ctx, h, &seat.vault_sk).expect("an honest share checks");
            SeatShare {
                seat: h.clone(),
                x,
                share,
            }
        })
        .collect()
}

#[test]
fn vault_key_differs_per_founding_anchor_and_is_stable() {
    let entropy = [9u8; 32];
    let one = derive_vault_seed(&entropy, "npk-1", "ipk");
    let again = derive_vault_seed(&entropy, "npk-1", "ipk");
    let other = derive_vault_seed(&entropy, "npk-2", "ipk");
    assert_eq!(*one, *again);
    assert_ne!(*one, *other);
    let (_, pk_one) = vault_keypair(&one);
    let (_, pk_again) = vault_keypair(&again);
    let (_, pk_other) = vault_keypair(&other);
    assert_eq!(pk_one, pk_again);
    assert_ne!(pk_one, pk_other);
    assert_ne!(*derive_vault_seed(&entropy, "npk-1", "ipk2"), *one);
    // the derived key is canonical
    assert_eq!(canonical_vault_pk(&pk_one), Ok(pk_one.clone()));
}

#[test]
fn key_seal_and_deposit_derivations_are_pinned() {
    let seed = derive_vault_seed(&[9; 32], "npk-1", "ipk");
    assert_eq!(
        hex::encode(*seed),
        "ac3565819df68ad5aa677b00963766eaa48732de813e44be00a2d39fa4bfd859"
    );
    let (_, pk) = vault_keypair(&seed);
    assert_eq!(
        pk,
        "3d675af49891d2e4be075d30fc8c1c6c60cd8ee1c7f1184484c360a2d5229345"
    );
    let sealed = seal_share(&pk, &Share::from_bytes([1; 32]), b"aad", &[3; 32]).expect("seals");
    assert_eq!(hex::encode(sealed), "90ab790ecbf0704232ffe9436faeccc913b66a60e99c688a40bba4d5e2e614509acb07e2e860a005ba2ae2acd3cfe9246c41a96ba69005caa13bfd618c4718473f83d46f97d4bc418b5fcb48b9f3b2ce");
    // the deal_seed / eph / secret chain end to end
    let s = seats(4);
    let c = ctx(&s, 2);
    let (dep, _) = deposit_by(&s, &c, 0, &text("one"), &mut rng(13));
    assert_eq!(dep.nonce, "48942ab2b0b2d4a671bbb5a579b3dde8");
    assert_eq!(
        dep.commitments,
        [
            "f80c96e8e2452db937dd1d4ef1ec6122486e0305b60c146be493af95566a5b39",
            "e4a7384722693ea986bbb52e8f3fa5cc9c1c3ec2900847ea01d5fa997f46c92d"
        ]
    );
    assert_eq!(dep.enc_share[0], "64d9e85d6c6af6063808456dcf8dc26e0bb54e34d66478a5b720b3eee91b483a0808e1f3171d8ebcc54ab164108f33905e6e96b462caa76a7e68f35ffc37ed7dc96fab6f5d3c85cc0b28c589c15bc0bb");
    assert_eq!(
        dep.payload.hash,
        "77ed454c1559f1ee86e5faa94d3949910a6061a2d92856884c508b29d845c8d3"
    );
    let (share, ikm) = rederive_share(&dep, RID, &c, &s[0].seed, "b").expect("re-deals");
    assert_eq!(
        hex::encode(share.as_bytes()),
        "31b5ff6791ebe71b07fcb05082e390cf3382fd5e10873d0fc17685760f827701"
    );
    assert_eq!(
        hex::encode(*ikm),
        "4e6422d3b50b9c0524f5441678d910a168df815dba1b80dbd0a9e41189f13d4e"
    );
}

/// Counts every draw hpke makes.
struct Counting {
    bytes: usize,
    calls: usize,
}

impl hpke::rand_core::RngCore for Counting {
    fn next_u32(&mut self) -> u32 {
        self.calls += 1;
        self.bytes += 4;
        7
    }
    fn next_u64(&mut self) -> u64 {
        self.calls += 1;
        self.bytes += 8;
        7
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.calls += 1;
        self.bytes += dst.len();
        dst.fill(7);
    }
}
impl hpke::rand_core::CryptoRng for Counting {}

#[test]
fn ikm_rng_draws_exactly_one_ikm() {
    use hpke::{Deserializable, Kem};
    let s = seats(1);
    let pk_raw = hex::decode(&s[0].vault_pk).expect("hex");
    let pk = <<hpke::kem::X25519HkdfSha256 as Kem>::PublicKey>::from_bytes(&pk_raw).expect("a key");
    let mut counting = Counting { bytes: 0, calls: 0 };
    hpke::single_shot_seal::<
        hpke::aead::ChaCha20Poly1305,
        hpke::kdf::HkdfSha256,
        hpke::kem::X25519HkdfSha256,
        _,
    >(
        &hpke::OpModeS::Base,
        &pk,
        b"",
        &[0u8; 32],
        b"aad",
        &mut counting,
    )
    .expect("seals");
    assert_eq!((counting.calls, counting.bytes), (1, 32));
    // and the seal itself goes through IkmRng, which panics on a second draw
    let share = Share::from_bytes([1; 32]);
    seal_share(&s[0].vault_pk, &share, b"aad", &[3; 32]).expect("seals with one draw");
}

#[test]
fn share_ciphertext_is_reproducible_from_share_and_ikm() {
    let s = seats(1);
    let share = Share::from_bytes([1; 32]);
    let one = seal_share(&s[0].vault_pk, &share, b"aad", &[3; 32]).expect("seals");
    let two = seal_share(&s[0].vault_pk, &share, b"aad", &[3; 32]).expect("seals");
    assert_eq!(one, two);
    assert_eq!(one.len(), SHARE_CT_LEN);
    assert_ne!(
        one,
        seal_share(&s[0].vault_pk, &share, b"aad", &[4; 32]).expect("seals")
    );
    assert_eq!(
        open_share(&s[0].vault_sk, &one, b"aad").expect("opens"),
        share
    );
    assert_eq!(
        open_share(&s[0].vault_sk, &one, b"other"),
        Err(VaultError::Open)
    );
}

#[test]
fn dealing_is_deterministic_and_pinned() {
    let one = deal(&[7; 32], 2, &[2, 3, 4]).expect("deals");
    let two = deal(&[7; 32], 2, &[2, 3, 4]).expect("deals");
    assert_eq!(one.commitments, two.commitments);
    assert_eq!(one.shares, two.shares);
    assert_eq!(one.s.commitment(), one.commitments[0]);
    let hexes: Vec<String> = one.commitments.iter().map(hex::encode).collect();
    assert_eq!(
        hexes,
        [
            "a2c3ad6f298c306d09550ee6f8bccaee2e43d36a4cb68a970ae45f4cc7116460",
            "ca3f9db3b7808bdd87faac3639b0da197e9b9daddd37973e2984586a24018e2a"
        ]
    );
    assert_eq!(
        hex::encode(one.shares[0].1.as_bytes()),
        "0c9756cbd35207eb206b1bfdd300c9b74cc72c7ad6393d77854b7a7528723207"
    );
    assert_ne!(
        deal(&[8; 32], 2, &[2, 3, 4]).expect("deals").commitments,
        one.commitments
    );
    assert_eq!(
        deal(&[7; 32], 1, &[2, 3]).err(),
        Some(VaultError::Threshold)
    );
    assert_eq!(
        deal(&[7; 32], 3, &[2, 3]).err(),
        Some(VaultError::Threshold)
    );
    assert_eq!(deal(&[7; 32], 2, &[2, 2]).err(), Some(VaultError::Deal));
    assert_eq!(deal(&[7; 32], 2, &[0, 2]).err(), Some(VaultError::Deal));
}

#[test]
fn two_deposits_of_one_text_deal_different_secrets() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let mut r = rng(1);
    let body = text("one");
    let (a, _) = deposit_by(&s, &c, 0, &body, &mut r);
    let (b, _) = deposit_by(&s, &c, 0, &body, &mut r);
    assert_ne!(a.nonce, b.nonce);
    assert_ne!(a.commitments, b.commitments);
    assert_ne!(secret_id(RID, &a), secret_id(RID, &b));
    let sa = recover_secret(&a, RID, &s[0].seed).expect("recovers");
    let sb = recover_secret(&b, RID, &s[0].seed).expect("recovers");
    assert_ne!(sa.commitment(), sb.commitment());
}

fn subsets(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    for mask in 0u32..(1 << n) {
        if usize::try_from(mask.count_ones()).expect("small") == k {
            out.push((0..n).filter(|i| mask & (1 << i) != 0).collect());
        }
    }
    out
}

#[test]
fn any_m_shares_reconstruct_and_m_minus_one_do_not() {
    for (n, m) in [(4u8, 2u8), (5, 3)] {
        let s = seats(n);
        let c = ctx(&s, m);
        // the depositor sits at position 2: holders' x skip it
        let body = text("hello vault");
        let (dep, file) = deposit_by(&s, &c, 1, &body, &mut rng(n));
        assert_eq!(dep.holders.len(), usize::from(n - 1));
        let shares = holder_shares(&s, &c, &dep);
        assert_eq!(
            shares.iter().map(|sh| sh.x).collect::<Vec<_>>(),
            (1..=n).filter(|x| *x != 2).collect::<Vec<_>>()
        );
        let want = recover_secret(&dep, RID, &s[1].seed)
            .expect("recovers")
            .commitment();
        for pick in subsets(shares.len(), usize::from(m)) {
            let chosen: Vec<SeatShare> = pick.iter().map(|i| shares[*i].clone()).collect();
            let pairs: Vec<(u8, Share)> = chosen.iter().map(|c| (c.x, c.share.clone())).collect();
            assert_eq!(combine(m, &pairs).expect("combines").commitment(), want);
            let opened = read(&dep, RID, &chosen, &file).expect("reads");
            assert_eq!(opened.text, body);
            assert!(opened.bad_seats.is_empty());
        }
        for pick in subsets(shares.len(), usize::from(m - 1)) {
            let chosen: Vec<SeatShare> = pick.iter().map(|i| shares[*i].clone()).collect();
            let fault = read(&dep, RID, &chosen, &file).expect_err("m-1 do not read");
            assert_eq!((fault.have, fault.need, fault.payload), (m - 1, m, false));
            if m > 2 {
                let pairs: Vec<(u8, Share)> =
                    chosen.iter().map(|c| (c.x, c.share.clone())).collect();
                assert_ne!(combine(m - 1, &pairs).expect("combines").commitment(), want);
            }
        }
    }
}

fn tampered(share: &Share) -> Share {
    let mut b = *share.as_bytes();
    b[0] ^= 1;
    Share::from_bytes(b)
}

#[test]
fn a_tampered_share_fails_feldman_and_names_its_seat() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let body = text("one");
    let (dep, file) = deposit_by(&s, &c, 0, &body, &mut rng(2));
    let mut shares = holder_shares(&s, &c, &dep);
    shares[0].share = tampered(&shares[0].share);
    let commitments: Vec<[u8; 32]> = dep
        .commitments
        .iter()
        .map(|h| {
            let mut out = [0u8; 32];
            hex::decode_to_slice(h, &mut out).expect("hex");
            out
        })
        .collect();
    assert!(!verify_share(&commitments, shares[0].x, &shares[0].share));
    // two good shares still read, and the bad one is named
    let opened = read(&dep, RID, &shares, &file).expect("reads with m good");
    assert_eq!(opened.text, body);
    assert_eq!(opened.bad_seats, ["b"]);
    // with one good share left: refused, still naming the seat
    let fault = read(&dep, RID, &shares[..2], &file).expect_err("too few");
    assert_eq!(fault.bad_seats, ["b"]);
    assert_eq!((fault.have, fault.need), (1, 2));
    // a share presented under another seat's x fails too
    let mut moved = holder_shares(&s, &c, &dep);
    moved[1].x = moved[2].x;
    assert!(!verify_share(&commitments, moved[1].x, &moved[1].share));
}

#[test]
fn a_malformed_commitment_blames_no_seat() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let (dep, file) = deposit_by(&s, &c, 0, &text("one"), &mut rng(12));
    let shares = holder_shares(&s, &c, &dep);
    let mut broken = dep.clone();
    broken.commitments[1] = "zz".repeat(32);
    let fault = read(&broken, RID, &shares, &file).expect_err("record fault");
    assert!(fault.bad_seats.is_empty());
    assert!(fault.record);
    assert_eq!((fault.have, fault.payload), (0, false));
}

#[test]
fn complaint_outcomes_name_the_right_party() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let (dep, _) = deposit_by(&s, &c, 0, &text("one"), &mut rng(3));
    let holder = "c";
    let hpk = &s[2].vault_pk;
    // honest record, honest reveal: the complaint was false
    let (share, ikm) = rederive_share(&dep, RID, &c, &s[0].seed, holder).expect("re-deals");
    assert_eq!(
        check_my_share(&dep, RID, &c, holder, &s[2].vault_sk).expect("checks"),
        share
    );
    assert_eq!(
        decide(&dep, RID, &c, holder, &share, &ikm),
        Ok(VaultRevealOutcome::FalseComplaint)
    );
    // honest record, lying reveal (another share, or another ikm): a lie
    assert_eq!(
        decide(&dep, RID, &c, holder, &tampered(&share), &ikm),
        Ok(VaultRevealOutcome::Lie)
    );
    assert_eq!(
        decide(&dep, RID, &c, holder, &share, &[0; 32]),
        Ok(VaultRevealOutcome::Lie)
    );
    // a record that dealt c a bad share, revealed truthfully: bad share
    let mut bad = dep.clone();
    let i = bad
        .holders
        .iter()
        .position(|h| h == holder)
        .expect("a holder");
    let wrong = tampered(&share);
    let sid = secret_id(RID, &bad);
    bad.enc_share[i] =
        hex::encode(seal_share(hpk, &wrong, &share_aad(&sid, holder), &ikm).expect("seals"));
    assert_eq!(
        check_my_share(&bad, RID, &c, holder, &s[2].vault_sk),
        Err(ShareFault::FailsFeldman)
    );
    assert_eq!(
        decide(&bad, RID, &c, holder, &wrong, &ikm),
        Ok(VaultRevealOutcome::BadShare)
    );
    // x and the key come from the founding context, never the caller
    assert_eq!(
        decide(&dep, RID, &c, "z", &share, &ikm),
        Err(VaultError::NotASeat("z".to_string()))
    );
    assert_eq!(
        decide(&dep, RID, &c, "a", &share, &ikm),
        Err(VaultError::NotASeat("a".to_string()))
    );
    let mut reordered = dep.clone();
    reordered.holders.swap(0, 1);
    assert_eq!(
        decide(&reordered, RID, &c, holder, &share, &ikm),
        Err(VaultError::Shape("holders"))
    );
    // the depositor itself holds no share
    assert_eq!(
        check_my_share(&dep, RID, &c, "a", &s[0].vault_sk),
        Err(ShareFault::NotAHolder)
    );
    // a key that is not the holder's does not open its share
    assert_eq!(
        check_my_share(&dep, RID, &c, holder, &s[3].vault_sk),
        Err(ShareFault::DoesNotOpen)
    );
}

#[test]
fn a_forged_depositor_fails_the_signature() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let pk = |i: usize| hex::encode(s[i].sk.verifying_key().to_bytes());
    let (dep, _) = deposit_by(&s, &c, 1, &text("one"), &mut rng(4));
    assert_eq!(verify_deposit_sig(&dep, RID, &pk(1)), Ok(()));
    // b's signature over a record naming a as depositor
    let mut forged = dep.clone();
    forged.depositor = "a".to_string();
    assert_eq!(
        verify_deposit_sig(&forged, RID, &pk(0)),
        Err(VaultError::Signature)
    );
    // any field changed after signing
    let mut renamed = dep.clone();
    renamed.name = "two".to_string();
    assert_eq!(
        verify_deposit_sig(&renamed, RID, &pk(1)),
        Err(VaultError::Signature)
    );
    assert_eq!(
        verify_deposit_sig(&dep, "r2", &pk(1)),
        Err(VaultError::Signature)
    );
}

#[test]
fn a_reordered_holder_list_fails_the_shape_check() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let (dep, _) = deposit_by(&s, &c, 0, &text("one"), &mut rng(5));
    assert_eq!(verify_deposit_shape(&dep, &c), Ok(()));
    let mut reordered = dep.clone();
    reordered.holders.swap(0, 1);
    assert_eq!(
        verify_deposit_shape(&reordered, &c),
        Err(VaultError::Shape("holders"))
    );
    let mut short = dep.clone();
    short.enc_share.pop();
    assert_eq!(
        verify_deposit_shape(&short, &c),
        Err(VaultError::Shape("enc_share"))
    );
    let mut m = dep.clone();
    m.m = 3;
    assert_eq!(verify_deposit_shape(&m, &c), Err(VaultError::Shape("m")));
    let mut upper = dep.clone();
    upper.nonce = upper.nonce.to_uppercase();
    assert_eq!(
        verify_deposit_shape(&upper, &c),
        Err(VaultError::Shape("nonce"))
    );
    let mut not_point = dep.clone();
    not_point.commitments[1] = "ff".repeat(32);
    assert_eq!(
        verify_deposit_shape(&not_point, &c),
        Err(VaultError::Shape("commitment"))
    );
    let mut stranger = dep.clone();
    stranger.depositor = "z".to_string();
    assert_eq!(
        verify_deposit_shape(&stranger, &c),
        Err(VaultError::NotASeat("z".to_string()))
    );
}

fn base_of(s: &[Seat], c: &VaultCtx) -> VaultBase {
    let (dep, _) = deposit_by(s, c, 0, &text("one"), &mut rng(6));
    let sid = secret_id(RID, &dep);
    let grant = VaultGrant {
        grant_id: grant_id(&sid, "b", 7),
        secret_id: sid,
        reader: "b".to_string(),
    };
    VaultBase {
        deposits: vec![VaultBaseDeposit {
            deposit: dep,
            grants: vec![VaultBaseGrant {
                proposal_id: 7,
                grant,
            }],
        }],
    }
}

#[test]
fn an_honest_base_verifies_and_round_trips() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let base = base_of(&s, &c);
    assert_eq!(verify_base(&base, RID, &c), Ok(()));
    let back =
        molt_core::vault::decode_vault_base(&molt_core::vault::vault_base_canonical_bytes(&base))
            .expect("decodes");
    assert_eq!(back, base);
    assert_eq!(verify_base(&back, RID, &c), Ok(()));
    assert_eq!(verify_base(&VaultBase::default(), RID, &c), Ok(()));
}

#[test]
fn a_base_with_a_bad_deposit_signature_is_refused() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let mut base = base_of(&s, &c);
    base.deposits[0].deposit.kind = "password".to_string();
    assert_eq!(verify_base(&base, RID, &c), Err(VaultError::Signature));
}

#[test]
fn a_base_with_a_wrong_grant_id_is_refused() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let mut base = base_of(&s, &c);
    base.deposits[0].grants[0].proposal_id = 8;
    assert!(matches!(
        verify_base(&base, RID, &c),
        Err(VaultError::Base(_))
    ));
    let mut stranger = base_of(&s, &c);
    let sid = stranger.deposits[0].grants[0].grant.secret_id.clone();
    stranger.deposits[0].grants[0].grant = VaultGrant {
        grant_id: grant_id(&sid, "z", 7),
        secret_id: sid,
        reader: "z".to_string(),
    };
    assert_eq!(
        verify_base(&stranger, RID, &c),
        Err(VaultError::NotASeat("z".to_string()))
    );
    let mut twice = base_of(&s, &c);
    let entry = twice.deposits[0].clone();
    twice.deposits.push(entry);
    assert!(matches!(
        verify_base(&twice, RID, &c),
        Err(VaultError::Base(_))
    ));
}

#[test]
fn payload_aad_binds_name_and_kind() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let body = text("one");
    let (dep, file) = deposit_by(&s, &c, 0, &body, &mut rng(7));
    let sc = recover_secret(&dep, RID, &s[0].seed).expect("recovers");
    assert_eq!(open_payload(&dep, RID, &sc, &file).expect("opens"), body);
    let key = dek(&sc, &molt_core::vault::dek_info(RID, "a", "one", "text"));
    assert_eq!(
        decrypt_payload(&key, &file, &payload_aad(RID, "a", "one", "text")).expect("opens"),
        body
    );
    for (name, kind) in [("two", "text"), ("one", "password")] {
        assert_eq!(
            decrypt_payload(&key, &file, &payload_aad(RID, "a", name, kind)),
            Err(VaultError::Open)
        );
    }
    assert_eq!(
        decrypt_payload(&key, &file, &payload_aad(RID, "b", "one", "text")),
        Err(VaultError::Open)
    );
    let mut renamed = dep.clone();
    renamed.name = "two".to_string();
    assert_eq!(
        open_payload(&renamed, RID, &sc, &file),
        Err(VaultError::Open)
    );
    // a file that is not the record's
    let mut other = file.clone();
    other[30] ^= 1;
    assert_eq!(open_payload(&dep, RID, &sc, &other), Err(VaultError::Open));
}

#[test]
fn resp_aad_binds_grant_and_seat() {
    let s = seats(4);
    let share = Share::from_bytes([5; 32]);
    let reader = &s[1];
    let sealed = seal_resp(
        &reader.vault_pk,
        &share,
        &resp_aad(RID, "gA", "c"),
        &mut rng(8),
    )
    .expect("seals");
    assert_eq!(
        open_resp(&reader.vault_sk, &sealed, &resp_aad(RID, "gA", "c")).expect("opens"),
        share
    );
    for aad in [
        resp_aad(RID, "gB", "c"),
        resp_aad(RID, "gA", "d"),
        resp_aad("r2", "gA", "c"),
    ] {
        assert_eq!(
            open_resp(&reader.vault_sk, &sealed, &aad),
            Err(VaultError::Open)
        );
    }
    // another seat's key opens nothing
    assert_eq!(
        open_resp(&s[2].vault_sk, &sealed, &resp_aad(RID, "gA", "c")),
        Err(VaultError::Open)
    );
    // fresh ephemerals: the same answer twice differs
    let again = seal_resp(
        &reader.vault_pk,
        &share,
        &resp_aad(RID, "gA", "c"),
        &mut rng(9),
    )
    .expect("seals");
    assert_ne!(sealed, again);
}

#[test]
fn max_payload_is_100_kib_plus_40() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let body = text(&"x".repeat(VAULT_PAYLOAD_MAX));
    let (dep, file) = deposit_by(&s, &c, 0, &body, &mut rng(10));
    assert_eq!(file.len(), 102_400 + 40);
    assert_eq!(PAYLOAD_OVERHEAD, 40);
    assert_eq!(dep.payload.size, 102_440);
    assert_eq!(verify_deposit_shape(&dep, &c), Ok(()));
    let shares = holder_shares(&s, &c, &dep);
    assert_eq!(read(&dep, RID, &shares, &file).expect("reads").text, body);
    let mut over = dep.clone();
    over.payload.size = 102_441;
    assert_eq!(
        verify_deposit_shape(&over, &c),
        Err(VaultError::Shape("payload size"))
    );
}

#[test]
fn oversize_text_is_refused() {
    let s = seats(4);
    let c = ctx(&s, 2);
    let body = text(&"x".repeat(VAULT_PAYLOAD_MAX + 1));
    let err = build_deposit(
        &DepositInput {
            republic_id: RID,
            depositor: "a",
            name: "one",
            kind: "text",
            text: &body,
            ctx: &c,
        },
        &s[0].seed,
        &s[0].sk,
        &mut rng(11),
    )
    .expect_err("refused");
    assert_eq!(err, VaultError::TooLarge);
    assert_eq!(
        encrypt_payload(&[0; 32], &body, b"", &[0; 24]),
        Err(VaultError::TooLarge)
    );
}

#[test]
fn canonical_vault_pk_rejects_uppercase_short_and_low_order() {
    let s = seats(1);
    let pk = s[0].vault_pk.clone();
    assert_eq!(canonical_vault_pk(&pk), Ok(pk.clone()));
    assert_eq!(canonical_vault_pk(&pk.to_uppercase()), Err(VaultError::Key));
    assert_eq!(canonical_vault_pk(&pk[..62]), Err(VaultError::Key));
    assert_eq!(canonical_vault_pk(&format!("{pk}00")), Err(VaultError::Key));
    assert_eq!(canonical_vault_pk(""), Err(VaultError::Key));
    assert_eq!(canonical_vault_pk(&"zz".repeat(32)), Err(VaultError::Key));
    // low order: u = 0, u = 1, u = p - 1
    let le = |head: &[u8], top: u8| {
        let mut b = [0u8; 32];
        b[..head.len()].copy_from_slice(head);
        b[31] |= top;
        hex::encode(b)
    };
    let mut p_minus_1 = [0xffu8; 32];
    p_minus_1[0] = 0xec;
    p_minus_1[31] = 0x7f;
    // the two order-8 points, and their u + p encodings (top bit set)
    let order_8 = [
        "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
        "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
        "cdeb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b880",
        "4c9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f11d7",
    ];
    for low in [le(&[0], 0), le(&[1], 0), hex::encode(p_minus_1)]
        .into_iter()
        .chain(order_8.map(str::to_string))
    {
        assert_eq!(canonical_vault_pk(&low), Err(VaultError::Key), "{low}");
    }
    // aliases: the top bit set on a valid key, and u + p of a valid u
    let mut top = hex::decode(&pk).expect("hex");
    top[31] |= 0x80;
    assert_eq!(canonical_vault_pk(&hex::encode(top)), Err(VaultError::Key));
    let two = le(&[2], 0);
    assert_eq!(canonical_vault_pk(&two), Ok(two.clone()));
    let mut two_plus_p = [0xffu8; 32];
    two_plus_p[0] = 0xef;
    two_plus_p[31] = 0x7f;
    assert_eq!(
        canonical_vault_pk(&hex::encode(two_plus_p)),
        Err(VaultError::Key)
    );
}
