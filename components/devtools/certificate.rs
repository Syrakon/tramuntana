/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The few fields of an X.509 certificate that the Security panel shows, read
//! straight from its DER encoding: who it was issued to and by, when it is
//! valid, its serial number and its fingerprints.

use chrono::{DateTime, NaiveDate, Utc};
use sha1::Sha1;
use sha2::{Digest, Sha256};

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const OID: u8 = 0x06;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
/// `[0] EXPLICIT`, the optional version of a TBSCertificate.
const VERSION: u8 = 0xa0;

/// `id-at-commonName`, `id-at-organizationName`, `id-at-organizationalUnitName`.
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const ORGANIZATION: &[u8] = &[0x55, 0x04, 0x0a];
const ORGANIZATIONAL_UNIT: &[u8] = &[0x55, 0x04, 0x0b];

/// The subject or the issuer of a certificate.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Identity {
    pub common_name: Option<String>,
    pub organization: Option<String>,
    pub organizational_unit: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Certificate {
    pub subject: Identity,
    pub issuer: Identity,
    pub not_before: Option<DateTime<Utc>>,
    pub not_after: Option<DateTime<Utc>>,
    /// Upper-case hex bytes joined by `:`, as Firefox shows them.
    pub serial_number: String,
    pub sha256: String,
    pub sha1: String,
}

/// One TLV of a DER encoding.
struct Element<'a> {
    tag: u8,
    content: &'a [u8],
}

/// Split the first element off `input`, or `None` when it is cut short or
/// uses an encoding DER does not allow.
fn element(input: &[u8]) -> Option<(Element<'_>, &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > std::mem::size_of::<usize>() || rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        let length = bytes.iter().fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (length, rest)
    };
    if rest.len() < length {
        return None;
    }
    let (content, rest) = rest.split_at(length);
    Some((Element { tag, content }, rest))
}

/// The elements of a constructed value, in order, up to the first bad one.
fn children(content: &[u8]) -> impl Iterator<Item = Element<'_>> {
    let mut rest = content;
    std::iter::from_fn(move || {
        let (next, after) = element(rest)?;
        rest = after;
        Some(next)
    })
}

fn colon_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// The attributes of a Name we know, each from its first occurrence.
fn identity(name: &Element) -> Identity {
    let mut out = Identity::default();
    for set in children(name.content).filter(|e| e.tag == SET) {
        for pair in children(set.content).filter(|e| e.tag == SEQUENCE) {
            let mut parts = children(pair.content);
            let (Some(oid), Some(value)) = (parts.next(), parts.next()) else {
                continue;
            };
            if oid.tag != OID {
                continue;
            }
            let slot = match oid.content {
                COMMON_NAME => &mut out.common_name,
                ORGANIZATION => &mut out.organization,
                ORGANIZATIONAL_UNIT => &mut out.organizational_unit,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(String::from_utf8_lossy(value.content).into_owned());
            }
        }
    }
    out
}

/// A UTCTime (`YYMMDDHHMMSSZ`) or a GeneralizedTime (`YYYYMMDDHHMMSSZ`).
fn time(element: &Element) -> Option<DateTime<Utc>> {
    let text = std::str::from_utf8(element.content).ok()?.strip_suffix('Z')?;
    let (year, rest) = match element.tag {
        // RFC 5280: two-digit years from 50 are in the 1900s.
        UTC_TIME => {
            let yy: i32 = text.get(..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, text.get(2..)?)
        },
        GENERALIZED_TIME => (text.get(..4)?.parse().ok()?, text.get(4..)?),
        _ => return None,
    };
    let field = |at: usize| -> Option<u32> { rest.get(at..at + 2)?.parse().ok() };
    let date = NaiveDate::from_ymd_opt(year, field(0)?, field(2)?)?;
    Some(date.and_hms_opt(field(4)?, field(6)?, field(8)?)?.and_utc())
}

/// Read a DER certificate; `None` when it is not one.
pub(crate) fn parse(der: &[u8]) -> Option<Certificate> {
    let (certificate, _) = element(der)?;
    let tbs = children(certificate.content).next().filter(|e| e.tag == SEQUENCE)?;
    let mut fields = children(tbs.content).peekable();
    if fields.peek().is_some_and(|e| e.tag == VERSION) {
        fields.next();
    }
    let serial = fields.next().filter(|e| e.tag == INTEGER)?;
    let _signature = fields.next()?;
    let issuer = fields.next().filter(|e| e.tag == SEQUENCE)?;
    let validity = fields.next().filter(|e| e.tag == SEQUENCE)?;
    let subject = fields.next().filter(|e| e.tag == SEQUENCE)?;
    let mut times = children(validity.content);
    let not_before = times.next().as_ref().and_then(time);
    let not_after = times.next().as_ref().and_then(time);
    // A positive INTEGER with its top bit set gets a leading zero byte.
    let serial_bytes = match serial.content {
        [0, rest @ ..] if !rest.is_empty() => rest,
        all => all,
    };
    Some(Certificate {
        subject: identity(&subject),
        issuer: identity(&issuer),
        not_before,
        not_after,
        serial_number: colon_hex(serial_bytes),
        sha256: colon_hex(&Sha256::digest(der)),
        sha1: colon_hex(&Sha1::digest(der)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap `content` in a TLV with `tag`.
    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if content.len() < 0x80 {
            out.push(content.len() as u8);
        } else {
            out.push(0x82);
            out.extend_from_slice(&(content.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(content);
        out
    }

    fn name(pairs: &[(&[u8], &str)]) -> Vec<u8> {
        let sets: Vec<u8> = pairs
            .iter()
            .flat_map(|(oid, value)| {
                let pair = [tlv(OID, oid), tlv(0x0c, value.as_bytes())].concat();
                tlv(SET, &tlv(SEQUENCE, &pair))
            })
            .collect();
        tlv(SEQUENCE, &sets)
    }

    fn certificate(serial: &[u8], not_before: &[u8], not_after: &[u8]) -> Vec<u8> {
        let tbs = [
            tlv(VERSION, &tlv(INTEGER, &[2])),
            tlv(INTEGER, serial),
            tlv(SEQUENCE, &tlv(OID, &[0x2a, 0x86, 0x48])),
            name(&[(COMMON_NAME, "Example CA"), (ORGANIZATION, "Example Org")]),
            tlv(SEQUENCE, &[not_before.to_vec(), not_after.to_vec()].concat()),
            name(&[(ORGANIZATION, "Site Inc"), (COMMON_NAME, "example.com"), (COMMON_NAME, "second")]),
            // A long subjectPublicKeyInfo, so the lengths take the long form.
            tlv(SEQUENCE, &[0u8; 300]),
        ]
        .concat();
        let signature_algorithm = tlv(SEQUENCE, &tlv(OID, &[0x2a]));
        tlv(SEQUENCE, &[tlv(SEQUENCE, &tbs), signature_algorithm, tlv(0x03, &[0, 1, 2])].concat())
    }

    #[test]
    fn reads_names_validity_and_serial() {
        let der = certificate(
            &[0x00, 0x9f, 0x01],
            &tlv(UTC_TIME, b"240131235959Z"),
            &tlv(GENERALIZED_TIME, b"20500101000000Z"),
        );
        let cert = parse(&der).expect("a well-formed certificate");
        assert_eq!(cert.subject.common_name.as_deref(), Some("example.com"));
        assert_eq!(cert.subject.organization.as_deref(), Some("Site Inc"));
        assert_eq!(cert.issuer.common_name.as_deref(), Some("Example CA"));
        assert_eq!(cert.issuer.organization.as_deref(), Some("Example Org"));
        assert_eq!(cert.serial_number, "9F:01");
        assert_eq!(cert.not_before.map(|t| t.to_rfc3339()).as_deref(), Some("2024-01-31T23:59:59+00:00"));
        assert_eq!(cert.not_after.map(|t| t.to_rfc3339()).as_deref(), Some("2050-01-01T00:00:00+00:00"));
        assert_eq!(cert.sha256.len(), 32 * 3 - 1);
        assert_eq!(cert.sha1.len(), 20 * 3 - 1);
    }

    #[test]
    fn two_digit_years_from_50_are_last_century() {
        let der = certificate(&[1], &tlv(UTC_TIME, b"500101000000Z"), &tlv(UTC_TIME, b"491231000000Z"));
        let cert = parse(&der).expect("a well-formed certificate");
        assert_eq!(cert.not_before.map(|t| t.to_rfc3339()).as_deref(), Some("1950-01-01T00:00:00+00:00"));
        assert_eq!(cert.not_after.map(|t| t.to_rfc3339()).as_deref(), Some("2049-12-31T00:00:00+00:00"));
    }

    #[test]
    fn cut_short_input_is_not_a_certificate() {
        let der = certificate(&[1], &tlv(UTC_TIME, b"240101000000Z"), &tlv(UTC_TIME, b"250101000000Z"));
        assert_eq!(parse(&der[..der.len() / 2]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[SEQUENCE, 0x85, 1, 2, 3, 4, 5]), None);
    }
}
