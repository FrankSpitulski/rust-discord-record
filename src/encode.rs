use std::process;

use byteorder::{ByteOrder, LittleEndian};
use ogg::PacketWriter;

use ogg_opus::Error;
use rand::Rng;

//--- Code ---------------------------------------------------------------------

const VER: &str = std::env!("CARGO_PKG_VERSION");

const fn to_samples<const S_PS: u32>(ms: u32) -> usize {
    ((S_PS * ms) / 1000) as usize
}

pub fn encode<const S_PS: u32, const NUM_CHANNELS: u8>(
    packets: &Vec<Vec<u8>>,
) -> Result<Vec<u8>, Error> {
    //NOTE: In the future the S_PS const generic will let us use const on a lot
    // of things, until then we need to use variables

    // This should have a bitrate of 24 Kb/s, exactly what IBM recommends

    // More frame time, slightly less overhead more problematic packet loses,
    // a frame time of 20ms is considered good enough for most applications

    // Data
    const FRAME_TIME_MS: u32 = 20;
    const OGG_OPUS_SPS: u32 = 48000;

    let frame_samples: usize = to_samples::<S_PS>(FRAME_TIME_MS);

    // Generate the serial which is nothing but a value to identify a stream, we
    // will also use the process id so that two programs don't use
    // the same serial even if getting one at the same time
    let mut rnd = rand::thread_rng();
    let serial = rnd.gen::<u32>() ^ process::id();
    let mut buffer: Vec<u8> = Vec::new();

    let mut packet_writer = PacketWriter::new(&mut buffer);

    let calc_samples = |counter: u32| -> usize { (counter as usize) * frame_samples };

    const fn granule<const S_PS: u32>(val: usize) -> u64 {
        const fn calc_sr_u64(val: u64, org_sr: u32, dest_sr: u32) -> u64 {
            (val * dest_sr as u64) / (org_sr as u64)
        }
        calc_sr_u64(val as u64, S_PS, OGG_OPUS_SPS)
    }

    #[rustfmt::skip]
    let opus_head: [u8; 19] = [
        b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', // Magic header
        1, // Version number, always 1
        NUM_CHANNELS, // Channels
        0, 0,//Pre-skip
        0, 0, 0, 0, // Original Hz (informational)
        0, 0, // Output gain
        0, // Channel map family
        // If Channel map != 0, here should go channel mapping table
    ];

    fn is_end_of_stream(end: bool) -> ogg::PacketWriteEndInfo {
        if end {
            ogg::PacketWriteEndInfo::EndStream
        } else {
            ogg::PacketWriteEndInfo::NormalPacket
        }
    }

    let mut head = opus_head;
    LittleEndian::write_u16(&mut head[10..12], 0u16); // Write pre-skip
    LittleEndian::write_u32(&mut head[12..16], S_PS); // Write Samples per second

    let mut opus_tags: Vec<u8> = Vec::with_capacity(60);
    let vendor_str = format!("ogg-opus {}", VER);
    opus_tags.extend(b"OpusTags");
    let mut len_bf = [0u8; 4];
    LittleEndian::write_u32(&mut len_bf, vendor_str.len() as u32);
    opus_tags.extend(&len_bf);
    opus_tags.extend(vendor_str.bytes());
    opus_tags.extend(&[0]); // No user comments

    packet_writer.write_packet(&head[..], serial, ogg::PacketWriteEndInfo::EndPage, 0)?;
    packet_writer.write_packet(opus_tags, serial, ogg::PacketWriteEndInfo::EndPage, 0)?;

    for i in 0..packets.len() {
        packet_writer.write_packet(
            &packets[i],
            serial,
            is_end_of_stream(i == packets.len() - 1),
            granule::<S_PS>(calc_samples((i + 1) as u32)),
        )?;
    }

    Ok(buffer)
}
