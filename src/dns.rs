const DNS_HEADER_SIZE: usize = 12;
const DNS_MAX_HOSTNAME_LEN: usize = 256;
const DNS_MAX_PACKET_SIZE: usize = 65_535;
const DNS_OFFSET_QUESTION: usize = DNS_HEADER_SIZE;
const DNS_TYPE_OPT: u16 = 41;

const DNS_RCODE_SERVFAIL: u8 = 2;
const DNS_RCODE_REFUSED: u8 = 5;

#[inline]
fn qdcount(packet: &[u8]) -> u16 {
    (u16::from(packet[4]) << 8) | u16::from(packet[5])
}

#[inline]
fn ancount(packet: &[u8]) -> u16 {
    (u16::from(packet[6]) << 8) | u16::from(packet[7])
}

#[inline]
fn nscount(packet: &[u8]) -> u16 {
    (u16::from(packet[8]) << 8) | u16::from(packet[9])
}

#[inline]
fn arcount(packet: &[u8]) -> u16 {
    (u16::from(packet[10]) << 8) | u16::from(packet[11])
}

#[inline]
pub fn rcode(packet: &[u8]) -> u8 {
    packet[3] & 0x0f
}

pub fn is_recoverable_error(packet: &[u8]) -> bool {
    let rcode = rcode(packet);
    rcode == DNS_RCODE_SERVFAIL || rcode == DNS_RCODE_REFUSED
}

fn arcount_inc(packet: &mut [u8]) -> Result<(), &'static str> {
    let mut arcount = arcount(packet);
    if arcount == 0xffff {
        return Err("Too many additional records");
    }
    arcount += 1;
    packet[10] = (arcount >> 8) as u8;
    packet[11] = arcount as u8;
    Ok(())
}

fn skip_name(packet: &[u8], offset: usize) -> Result<(usize, u16), &'static str> {
    let packet_len = packet.len();
    if offset >= packet_len - 1 {
        return Err("Short packet");
    }
    let mut name_len: usize = 0;
    let mut offset = offset;
    let mut labels_count = 0u16;
    loop {
        let label_len = match packet[offset] {
            len if len & 0xc0 == 0xc0 => {
                if 2 > packet_len - offset {
                    return Err("Incomplete offset");
                }
                offset += 2;
                break;
            }
            len if len > 0x3f => return Err("Label too long"),
            len => len,
        } as usize;
        if label_len >= packet_len - offset - 1 {
            return Err("Malformed packet with an out-of-bounds name");
        }
        name_len += label_len + 1;
        if name_len > DNS_MAX_HOSTNAME_LEN {
            return Err("Name too long");
        }
        offset += label_len + 1;
        if label_len == 0 {
            break;
        }
        labels_count += 1;
    }
    Ok((offset, labels_count))
}

fn traverse_rrs<F: FnMut(usize) -> Result<(), &'static str>>(
    packet: &[u8],
    mut offset: usize,
    rrcount: u16,
    mut cb: F,
) -> Result<usize, &'static str> {
    let packet_len = packet.len();
    for _ in 0..rrcount {
        offset = match skip_name(packet, offset) {
            Ok(offset) => offset.0,
            Err(e) => return Err(e),
        };
        if 10 > packet_len - offset {
            return Err("Short packet");
        }
        cb(offset)?;
        let rdlen = (u16::from(packet[offset + 8]) << 8 | u16::from(packet[offset + 9])) as usize;
        offset += 10;
        if rdlen > packet_len - offset {
            return Err("Record length would exceed packet length");
        }
        offset += rdlen;
    }
    Ok(offset)
}

fn traverse_rrs_mut<F: FnMut(&mut [u8], usize) -> Result<(), &'static str>>(
    packet: &mut [u8],
    mut offset: usize,
    rrcount: u16,
    mut cb: F,
) -> Result<usize, &'static str> {
    let packet_len = packet.len();
    for _ in 0..rrcount {
        offset = match skip_name(packet, offset) {
            Ok(offset) => offset.0,
            Err(e) => return Err(e),
        };
        if 10 > packet_len - offset {
            return Err("Short packet");
        }
        cb(packet, offset)?;
        let rdlen = (u16::from(packet[offset + 8]) << 8 | u16::from(packet[offset + 9])) as usize;
        offset += 10;
        if rdlen > packet_len - offset {
            return Err("Record length would exceed packet length");
        }
        offset += rdlen;
    }
    Ok(offset)
}

pub(crate) fn min_ttl(
    packet: &[u8],
    min_ttl: u32,
    max_ttl: u32,
    failure_ttl: u32,
) -> Result<u32, &'static str> {
    if qdcount(packet) != 1 {
        return Err("Unsupported number of questions");
    }
    let packet_len = packet.len();
    if packet_len <= DNS_OFFSET_QUESTION {
        return Err("Short packet");
    }
    if packet_len >= DNS_MAX_PACKET_SIZE {
        return Err("Large packet");
    }
    let mut offset = match skip_name(packet, DNS_OFFSET_QUESTION) {
        Ok(offset) => offset.0,
        Err(e) => return Err(e),
    };
    assert!(offset > DNS_OFFSET_QUESTION);
    if 4 > packet_len - offset {
        return Err("Short packet");
    }
    offset += 4;
    let ancount = ancount(packet);
    let nscount = nscount(packet);
    let arcount = arcount(packet);
    let rrcount = ancount + nscount + arcount;

    let mut found_min_ttl = if rrcount > 0 { max_ttl } else { failure_ttl };
    offset = traverse_rrs(packet, offset, rrcount, |offset| {
        let qtype = u16::from(packet[offset]) << 8 | u16::from(packet[offset + 1]);
        let ttl = u32::from(packet[offset + 4]) << 24
            | u32::from(packet[offset + 5]) << 16
            | u32::from(packet[offset + 6]) << 8
            | u32::from(packet[offset + 7]);
        if qtype != DNS_TYPE_OPT && ttl < found_min_ttl {
            found_min_ttl = ttl;
        }
        Ok(())
    })?;
    if found_min_ttl < min_ttl {
        found_min_ttl = min_ttl;
    }
    if offset != packet_len {
        return Err("Garbage after packet");
    }
    Ok(found_min_ttl)
}

fn add_edns_section(packet: &mut Vec<u8>, max_payload_size: u16) -> Result<(), &'static str> {
    let opt_rr: [u8; 11] = [
        0,
        (DNS_TYPE_OPT >> 8) as u8,
        DNS_TYPE_OPT as u8,
        (max_payload_size >> 8) as u8,
        max_payload_size as u8,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    if DNS_MAX_PACKET_SIZE - packet.len() < opt_rr.len() {
        return Err("Packet would be too large to add a new record");
    }
    arcount_inc(packet)?;
    packet.extend(&opt_rr);
    Ok(())
}

pub(crate) fn set_edns_max_payload_size(
    packet: &mut Vec<u8>,
    max_payload_size: u16,
) -> Result<(), &'static str> {
    if qdcount(packet) != 1 {
        return Err("Unsupported number of questions");
    }
    let packet_len = packet.len();
    if packet_len <= DNS_OFFSET_QUESTION {
        return Err("Short packet");
    }
    if packet_len >= DNS_MAX_PACKET_SIZE {
        return Err("Large packet");
    }
    let mut offset = match skip_name(packet, DNS_OFFSET_QUESTION) {
        Ok(offset) => offset.0,
        Err(e) => return Err(e),
    };
    assert!(offset > DNS_OFFSET_QUESTION);
    if 4 > packet_len - offset {
        return Err("Short packet");
    }
    offset += 4;
    let ancount = ancount(packet);
    let nscount = nscount(packet);
    let arcount = arcount(packet);

    offset = traverse_rrs(packet, offset, ancount + nscount, |_offset| Ok(()))?;

    let mut edns_payload_set = false;
    traverse_rrs_mut(packet, offset, arcount, |packet, offset| {
        let qtype = u16::from(packet[offset]) << 8 | u16::from(packet[offset + 1]);
        if qtype == DNS_TYPE_OPT {
            if edns_payload_set {
                return Err("Duplicate OPT RR found");
            }
            packet[offset + 2] = (max_payload_size >> 8) as u8;
            packet[offset + 3] = max_payload_size as u8;
            edns_payload_set = true;
        }
        Ok(())
    })?;

    if edns_payload_set {
        return Ok(());
    }
    add_edns_section(packet, max_payload_size)?;

    Ok(())
}
