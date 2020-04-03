use std::error::Error;
use std::str;
use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::prelude::*;
use tokio::spawn;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

pub async fn handle(stream: TcpStream) {
    println!("ws open");
    let (input, output) = io::split(stream);
    let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel(16);

    spawn(async move {
        if let Err(e) = write(output, rx).await {
            println!("ws_write error {:?}", e);
        }
        println!("write half closed");
    });
    spawn(async move {
        if let Err(e) = read(input, tx).await {
            println!("ws_read error {:?}", e);
        }
        println!("read half closed");
    });
}

async fn read(mut input: ReadHalf<TcpStream>, mut rx: Sender<Vec<u8>>) -> Result<(), Box<dyn Error>> {
    loop {
        let mut buf = [0u8; 2];
        if let Err(e) = input.read_exact(&mut buf).await {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                break; //tcp closed without ws close handshake
            }
            return Err(Box::new(e));
        }
        //println!("1: {:02x?}", buf);

        let mut header = Header::new(buf[0], buf[1]);
        let rn = header.read_next() as usize;
        if rn > 0 {
            let mut buf = vec![0u8; rn];
            input.read_exact(&mut buf).await?;
            header.set_header(&buf);
            //println!("2: {:02x?}", buf);
        }

        let rn = header.payload_len as usize;
        if rn > 0 {
            let mut buf = vec![0u8; rn];
            input.read_exact(&mut buf).await?;
            header.set_payload(&buf);
            //println!("3: {:02x?}", buf);
        }

        if !header.is_ok() {
            break;
        }
        //println!("3 header: {:?}", header);
        match header.kind() {
            FrameKind::Text => {
                //println!("ws body {} bytes, as str: {}", rn, header.payload_str());
                println!("ws body {} bytes", rn);
                rx.send(text_message(header.payload_str())).await?;
            }
            FrameKind::Binary => {
                println!("ws body is binary frame of size {}", header.payload_len);
                rx.send(binary_message(&header.payload)).await?;
            }
            FrameKind::Close => {
                println!("ws close");
                rx.send(close_message()).await?;
                break;
            }
            FrameKind::Ping => {
                println!("ws ping");
                rx.send(header.to_pong()).await?;
            }
            FrameKind::Pong => println!("ws pong"),
            FrameKind::Continuation => {
                return Err("ws continuation frame not supported".into());
            }
            FrameKind::Reserved(opcode) => {
                return Err(format!("reserved ws frame opcode {}", opcode).into());
            }
        }
    }

    Ok(())
}

async fn write(mut output: WriteHalf<TcpStream>, mut rx: Receiver<Vec<u8>>) -> io::Result<()> {
    while let Some(v) = rx.recv().await {
        output.write(&v).await?;
    }
    Ok(())
}

#[derive(Debug)]
struct Header {
    #[allow(dead_code)]
    fin: bool,
    #[allow(dead_code)]
    rsv1: bool,
    #[allow(dead_code)]
    rsv2: bool,
    #[allow(dead_code)]
    rsv3: bool,
    rsv: u8,
    mask: bool,
    opcode: u8,
    payload_len: u64,
    masking_key: [u8; 4],
    payload: Vec<u8>,
}

enum FrameKind {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
    Reserved(u8),
}

impl Header {
    fn new(byte1: u8, byte2: u8) -> Header {
        Header {
            fin: byte1 & 0b1000_0000u8 != 0,
            rsv1: byte1 & 0b0100_0000u8 != 0,
            rsv2: byte1 & 0b0010_0000u8 != 0,
            rsv3: byte1 & 0b0001_0000u8 != 0,
            rsv: (byte1 & 0b0111_0000u8) >> 4,
            opcode: byte1 & 0b0000_1111u8,
            mask: byte2 & 0b1000_0000u8 != 0,
            payload_len: (byte2 & 0b0111_1111u8) as u64,
            masking_key: [0; 4],
            payload: vec![0; 0],
        }
    }
    fn read_next(&self) -> u8 {
        let mut n: u8 = if self.mask { 4 } else { 0 };
        if self.payload_len >= 126 {
            n += 2;
        }
        if self.payload_len == 127 {
            n += 6;
        }
        n
    }
    fn set_header(&mut self, buf: &[u8]) {
        let mask_start = buf.len() - 4;
        if mask_start == 8 {
            let bytes: [u8; 8] = [buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]];
            self.payload_len = u64::from_be_bytes(bytes);
        }
        if mask_start == 2 {
            let bytes: [u8; 2] = [buf[0], buf[1]];
            self.payload_len = u16::from_be_bytes(bytes) as u64;
        }
        for i in 0..4 {
            self.masking_key[i] = buf[mask_start + i];
        }
    }
    fn set_payload(&mut self, buf: &[u8]) {
        let mut decoded = vec![0u8; self.payload_len as usize];
        for (i, b) in buf.iter().enumerate() {
            decoded[i] = b ^ self.masking_key[i % 4];
        }
        self.payload = decoded;
    }
    fn payload_str(&self) -> &str {
        match str::from_utf8(&self.payload) {
            Ok(v) => v,
            _ => "",
        }
    }
    fn kind(&self) -> FrameKind {
        match self.opcode {
            0 => FrameKind::Continuation,
            1 => FrameKind::Text,
            2 => FrameKind::Binary,
            8 => FrameKind::Close,
            9 => FrameKind::Ping,
            0xa => FrameKind::Pong,
            _ => FrameKind::Reserved(self.opcode),
        }
    }
    fn is_control_frame(&self) -> bool {
        match self.kind() {
            FrameKind::Continuation | FrameKind::Close | FrameKind::Ping | FrameKind::Pong => true,
            _ => false,
        }
    }
    fn is_rsv_ok(&self) -> bool {
        // rsv must be 0, when no extension defining RSV meaning has been negotiated
        self.rsv == 0
    }
    fn is_ok(&self) -> bool {
        if self.is_control_frame() && self.payload_len > 125 {
            return false;
        }
        if self.is_control_frame() && !self.fin {
            return false; // Control frames themselves MUST NOT be fragmented.
        }
        if self.fin && self.opcode == 0 {
            return false; // fin frame must have opcode
        }
        if !self.fin && self.opcode != 0 {
            return false; // non fin frame must have continuation opcode
        }
        self.is_rsv_ok()
    }
    fn to_pong(&self) -> Vec<u8> {
        //vec![0b1000_1010u8, 0b00000000u8]
        create_message(FrameKind::Pong, &self.payload)
    }
}

fn close_message() -> Vec<u8> {
    vec![0b1000_1000u8, 0b0000_0000u8]
}

fn text_message(text: &str) -> Vec<u8> {
    create_message(FrameKind::Text, text.as_bytes())
}

#[allow(dead_code)]
fn binary_message(data: &[u8]) -> Vec<u8> {
    create_message(FrameKind::Binary, data)
}

/*
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-------+-+-------------+-------------------------------+
|F|R|R|R| opcode|M| Payload len |    Extended payload length    |
|I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
|N|V|V|V|       |S|             |   (if payload len==126/127)   |
| |1|2|3|       |K|             |                               |
+-+-+-+-+-------+-+-------------+ - - - - - - - - - - - - - - - +
|     Extended payload length continued, if payload len == 127  |
+ - - - - - - - - - - - - - - - +-------------------------------+
|                               |Masking-key, if MASK set to 1  |
+-------------------------------+-------------------------------+
| Masking-key (continued)       |          Payload Data         |
+-------------------------------- - - - - - - - - - - - - - - - +
:                     Payload Data continued ...                :
+ - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - +
|                     Payload Data continued ...                |
+---------------------------------------------------------------+
*/
fn create_message(kind: FrameKind, body: &[u8]) -> Vec<u8> {
    let mut first = 0b1000_0000u8;
    first += match kind {
        FrameKind::Text => 1,
        FrameKind::Binary => 2,
        FrameKind::Close => 8,
        FrameKind::Ping => 9,
        FrameKind::Pong => 0xa,
        _ => 0,
    };
    let mut buf = vec![first];

    // add peyload length
    let l = body.len();
    if l < 126 {
        buf.push(l as u8);
    } else if body.len() < 65536 {
        buf.push(126u8);
        let l = l as u16;
        buf.extend_from_slice(&l.to_be_bytes());
    } else {
        buf.push(127u8);
        let l = l as u64;
        buf.extend_from_slice(&l.to_be_bytes());
    }

    buf.extend_from_slice(body);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_text_message() {
        let buf = text_message("abc");
        assert_eq!(5, buf.len());
        assert_eq!([0x81, 0x03, 0x61, 0x62, 0x63], buf[0..]);

        let buf = text_message("The length of the Payload data, in bytes: if 0-125, that is the payload length.  If 126, the following 2 bytes interpreted as a 16-bit unsigned integer are the payload length.");
        assert_eq!(179, buf.len());
        assert_eq!([0x81, 0x7e, 0x00, 0xaf], buf[0..4]);
        assert_eq!(0xaf, buf[3]); // 175 is body length

        //println!("{:02x?}", buf);
    }
}
