mod data_element;
mod error;
mod service;

use std::collections::BTreeMap;
use std::mem::size_of;
use std::ops::RangeInclusive;
use std::sync::Arc;
use bytes::{Bytes, BytesMut};
use instructor::{BigEndian, Buffer, BufferMut, Exstruct, Instruct};
use instructor::utils::Length;
use tokio::spawn;
use tracing::{error, trace, warn};
use crate::{ensure, hci};
use crate::l2cap::channel::Channel;
use crate::l2cap::{AVDTP_PSM, Server};
use crate::sdp::error::{Error, SdpErrorCodes};
use crate::sdp::service::{Service, ServiceAttribute};

pub use data_element::{DataElement, Uuid};

/*
#[derive(Default, Clone)]
pub struct SdpServerBuilder {
    records: BTreeMap<Uuid, BTreeMap<u16, DataElement>>
}

impl SdpServerBuilder {
    pub fn add_records(mut self, service: impl Into<Uuid>, records: impl IntoIterator<Item=(u16, DataElement)>) -> Self {
        self
            .records
            .entry(service.into())
            .or_default()
            .extend(records.into_iter().map(|(id, value)| (id, value.into())));
        self
    }

    pub fn build(self) -> SdpServer {
        SdpServer {
            records: Arc::new(self.records)
        }
    }
}
*/



#[derive(Clone)]
pub struct SdpServer {
    records: Arc<BTreeMap<u32, Service>>
}

const SDP_SERVICE_RECORD_HANDLE_ATTRIBUTE_ID: u16 = 0x0000;
const SDP_SERVICE_CLASS_ID_LIST_ATTRIBUTE_ID: u16 = 0x0001;
const SDP_PROTOCOL_DESCRIPTOR_LIST_ATTRIBUTE_ID: u16 = 0x0004;
const SDP_BROWSE_GROUP_LIST_ATTRIBUTE_ID: u16 = 0x0005;
const SDP_BLUETOOTH_PROFILE_DESCRIPTOR_LIST_ATTRIBUTE_ID: u16 = 0x0009;


const SDP_PUBLIC_BROWSE_ROOT: Uuid = Uuid::from_u16(0x1002);
const BT_AUDIO_SINK_SERVICE: Uuid = Uuid::from_u16(0x110b);
const BT_L2CAP_PROTOCOL_ID: Uuid = Uuid::from_u16(0x0100);
const BT_AVDTP_PROTOCOL_ID: Uuid = Uuid::from_u16(0x0019);
const BT_ADVANCED_AUDIO_DISTRIBUTION_SERVICE: Uuid = Uuid::from_u16(0x110d);

impl Default for SdpServer {
    fn default() -> Self {
        //SdpServerBuilder::default()
        //    .add_records(PNP_INFORMATION, [])
        //    .build()
        let service_record_handle = 0x00010001;
        let version = 1u16 << 8 | 3u16;
        SdpServer {
            records: Arc::new(BTreeMap::from_iter([
                (service_record_handle, Service::from_iter([
                    ServiceAttribute::new(SDP_SERVICE_RECORD_HANDLE_ATTRIBUTE_ID, service_record_handle),
                    ServiceAttribute::new(SDP_BROWSE_GROUP_LIST_ATTRIBUTE_ID, DataElement::from_iter([
                        SDP_PUBLIC_BROWSE_ROOT,
                    ])),
                    ServiceAttribute::new(SDP_SERVICE_CLASS_ID_LIST_ATTRIBUTE_ID, DataElement::from_iter([
                        BT_AUDIO_SINK_SERVICE,
                    ])),
                    ServiceAttribute::new(SDP_PROTOCOL_DESCRIPTOR_LIST_ATTRIBUTE_ID, DataElement::from_iter([
                        (BT_L2CAP_PROTOCOL_ID, AVDTP_PSM),
                        (BT_AVDTP_PROTOCOL_ID, version)
                    ])),
                    ServiceAttribute::new(SDP_BLUETOOTH_PROFILE_DESCRIPTOR_LIST_ATTRIBUTE_ID, DataElement::from_iter([
                        (BT_ADVANCED_AUDIO_DISTRIBUTION_SERVICE, version)
                    ])),
                ]))
            ])),
        }
    }
}

impl Server for SdpServer {

    fn on_connection(&mut self, mut channel: Channel) {
        let server = self.clone();
        spawn(async move {
            if let Err(err) = channel.configure().await {
                warn!("Error configuring channel: {:?}", err);
                return;
            }
            server.handle_connection(channel).await.unwrap_or_else(|err| {
                warn!("Error handling connection: {:?}", err);
            });
            trace!("SDP connection closed");
        });
    }

}

impl SdpServer {

    async fn handle_connection(self, mut channel: Channel) -> Result<(), hci::Error> {
        let mut buffer = BytesMut::new();
        while let Some(mut request) = channel.read().await {
            let Ok(SdpHeader {pdu, transaction_id, ..}) = request
                .read()
                .map_err(|err| error!("malformed request: {}", err))
                else {
                continue;
            };
            let reply = catch_error(|| match pdu {
                // ([Vol 3] Part B, Section 4.5.1).
                PduId::SearchRequest => {
                    let service_search_patterns: DataElement = request.read()?;
                    let maximum_service_record_count: u16 = request.read_be()?;
                    let cont: ContinuationState = request.read_be()?;
                    request.finish()?;
                    // We don't have to split this packet into multiple responses if we don't want to (we don't want to)
                    ensure!(cont == ContinuationState::None, Error::InvalidContinuationState);

                    let service_search_patterns = convert_search_pattern(service_search_patterns)?;
                    let attribute_list = self
                        .collecting_matching_records(&service_search_patterns)
                        .map(|(id, _)| *id)
                        .take(maximum_service_record_count as usize)
                        .collect::<Vec<_>>();

                    Ok(ResponsePacket::SearchResponse {
                        total_service_record_count: maximum_service_record_count,
                        current_service_record_count: maximum_service_record_count,
                        service_record_handles: attribute_list,
                        continuation_state: ContinuationState::None,
                    })
                },
                // ([Vol 3] Part B, Section 4.6.1).
                PduId::AttributeRequest => {
                    let service_record_handle: u32 = request.read_be()?;
                    let maximum_attribute_byte_count: u16 = request.read_be()?;
                    let attribute_id_list: DataElement = request.read()?;
                    let cont: ContinuationState = request.read_be()?;
                    request.finish()?;

                    match cont {
                        ContinuationState::None => {
                            buffer.clear();

                            let attributes_id_list = convert_attribute_id_list(attribute_id_list)?;

                            let attribute_list = self
                                .records
                                .get(&service_record_handle)
                                .map(|service| collect_attributes(service, &attributes_id_list))
                                .ok_or(Error::UnknownServiceRecordHandle(service_record_handle))?;

                            buffer.write(&attribute_list);
                        },
                        ContinuationState::Continue => {
                            ensure!(!buffer.is_empty(), Error::InvalidContinuationState);
                        }
                    }
                    let to_send = buffer.split_to(buffer.len().min(maximum_attribute_byte_count as usize));
                    Ok(ResponsePacket::AttributeResponse {
                        attribute_list_size: to_send.len() as u16,
                        attribute_list: to_send.freeze(),
                        continuation_state: ContinuationState::last_message(buffer.is_empty())
                    })
                },
                // ([Vol 3] Part B, Section 4.7.1).
                PduId::SearchAttributeRequest => {
                    let service_search_patterns: DataElement = request.read()?;
                    let max_attr_len: usize = request.read_be::<u16>()? as usize;
                    let attributes: DataElement = request.read()?;
                    let cont: ContinuationState = request.read_be()?;
                    request.finish()?;

                    match cont {
                        ContinuationState::None => {
                            buffer.clear();

                            let service_search_patterns = convert_search_pattern(service_search_patterns)?;
                            let attributes_id_list = convert_attribute_id_list(attributes)?;

                            let attribute_list = self
                                .collecting_matching_records(&service_search_patterns)
                                .map(|(_, service)| collect_attributes(service, &attributes_id_list))
                                .filter(|element| !element.is_empty())
                                .collect::<DataElement>();

                            buffer.write(&attribute_list);
                        },
                        ContinuationState::Continue => {
                            ensure!(!buffer.is_empty(), Error::InvalidContinuationState);
                        }
                    }
                    let to_send = buffer.split_to(max_attr_len.min(buffer.len()));
                    Ok(ResponsePacket::SearchAttributeResponse {
                        attribute_list_size: to_send.len() as u16,
                        attribute_list: to_send.freeze(),
                        continuation_state: ContinuationState::last_message(buffer.is_empty())
                    })
                }
                _ => {
                    warn!("Unsupported PDU: {:?}", pdu);
                    Err(Error::InvalidRequest)
                }
            }).unwrap_or_else(|err| {
                error!("Error handling request: {:?}", err);
                ResponsePacket::ErrorResponse(SdpErrorCodes::from(err))
            });
            let mut packet = BytesMut::new();
            packet.write(&SdpHeader {
                pdu: reply.pdu(),
                transaction_id,
                parameter_length: Length::new(reply.byte_size())?,
            });
            packet.write(&reply);
            channel.write(packet.freeze())?;
        }
        Ok(())
    }

    fn collecting_matching_records<'a: 'b, 'b>(&'a self, service_search_patterns: &'b [Uuid]) -> impl Iterator<Item=(&'a u32, &'a Service)> + 'b {
        self
            .records
            .iter()
            .filter(move |(_, service)| service_search_patterns
                .iter()
                .any(|&uuid| service.contains(uuid)))
    }

}

fn collect_attributes(service: &Service, attribute_id_list: &[RangeInclusive<u16>]) -> DataElement {
    service
        .attributes(attribute_id_list)
        .filter(|attribute| attribute.value != DataElement::Nil)
        .cloned()
        .flat_map(ServiceAttribute::into_iter)
        .collect::<DataElement>()
}

fn convert_search_pattern(pattern: DataElement) -> Result<Vec<Uuid>, Error> {
    pattern
        .as_sequence()?
        .iter()
        .map(|element| element.as_uuid())
        .collect::<Result<Vec<_>, _>>()
}

fn convert_attribute_id_list(list: DataElement) -> Result<Vec<RangeInclusive<u16>>, Error> {
    list
        .as_sequence()?
        .iter()
        .map(|element| match element {
            DataElement::U16(id) => Ok(*id..=*id),
            DataElement::U32(range) => {
                let start = (*range >> 16) as u16;
                let end = (*range & 0xFFFF) as u16;
                Ok(start..=end)
            }
            _ => Err(Error::UnexpectedDataType)
        })
        .collect::<Result<Vec<_>, _>>()
}

fn catch_error<F, E, R>(f: F) -> Result<R, E>
    where F: FnOnce() -> Result<R, E>
{
    f()
}

#[derive(Debug, Exstruct, Instruct)]
#[instructor(endian = "big")]
struct SdpHeader {
    pdu: PduId,
    transaction_id: u16,
    parameter_length: Length<u16, 0>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Exstruct, Instruct)]
#[repr(u8)]
enum PduId {
    ErrorResponse = 0x01,
    SearchRequest = 0x02,
    SearchResponse = 0x03,
    AttributeRequest = 0x04,
    AttributeResponse = 0x05,
    SearchAttributeRequest = 0x06,
    SearchAttributeResponse = 0x07,
}

#[derive(Debug, Instruct)]
#[instructor(endian = "big")]
enum ResponsePacket {
    // ([Vol 3] Part B, Section 4.4.1).
    ErrorResponse(SdpErrorCodes),
    // ([Vol 3] Part B, Section 4.5.2).
    SearchResponse {
        total_service_record_count: u16,
        current_service_record_count: u16,
        service_record_handles: Vec<u32>,
        continuation_state: ContinuationState
    },
    // ([Vol 3] Part B, Section 4.6.2).
    AttributeResponse {
        attribute_list_size: u16,
        attribute_list: Bytes,
        continuation_state: ContinuationState
    },
    // ([Vol 3] Part B, Section 4.7.2).
    SearchAttributeResponse {
        attribute_list_size: u16,
        attribute_list: Bytes,
        continuation_state: ContinuationState
    }
}

impl ResponsePacket {
    pub fn pdu(&self) -> PduId {
        match self {
            Self::ErrorResponse(_) => PduId::ErrorResponse,
            Self::SearchResponse{ .. } => PduId::SearchResponse,
            Self::AttributeResponse{ .. } => PduId::AttributeResponse,
            Self::SearchAttributeResponse { .. } => PduId::SearchAttributeResponse
        }
    }

    pub fn byte_size(&self) -> usize {
        match self {
            Self::ErrorResponse(_) => size_of::<SdpErrorCodes>(),
            Self::SearchResponse { service_record_handles, continuation_state, ..} => {
                2 * size_of::<u16>() + service_record_handles.len() * size_of::<u32>() + continuation_state.byte_size()
            },
            Self::AttributeResponse {attribute_list, continuation_state, ..} => {
                size_of::<u16>() + attribute_list.len() + continuation_state.byte_size()
            },
            Self::SearchAttributeResponse { attribute_list, continuation_state , .. } => {
                size_of::<u16>() + attribute_list.len() + continuation_state.byte_size()
            }
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ContinuationState {
    None,
    Continue
}

impl ContinuationState {

    const CONTINUATION_STATE: [u8; 4] = *b"cont";

    pub fn last_message(last: bool) -> Self {
        last.then_some(Self::None).unwrap_or(Self::Continue)
    }

    pub fn byte_size(self) -> usize {
        match self {
            Self::None => 1,
            Self::Continue => 5
        }
    }
}

impl Exstruct<BigEndian> for ContinuationState {
    fn read_from_buffer<B: Buffer>(buffer: &mut B) -> Result<Self, instructor::Error> {
        let len: u8 = buffer.read_be()?;
        match len {
            0 => Ok(Self::None),
            4 => {
                ensure!(buffer.read_be::<[u8; 4]>()? == Self::CONTINUATION_STATE, instructor::Error::InvalidValue);
                Ok(Self::Continue)
            },
            _ => Err(instructor::Error::InvalidValue)
        }
    }
}

impl Instruct<BigEndian> for ContinuationState {
    fn write_to_buffer<B: BufferMut>(&self, buffer: &mut B) {
        match self {
            Self::None => buffer.write_be(&0u8),
            Self::Continue => {
                buffer.write_be(&4u8);
                buffer.write_be(&Self::CONTINUATION_STATE);
            }
        }
    }
}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{Buf, Bytes};
    use instructor::Buffer;
    use crate::sdp::data_element::{DataElement};

    #[test]
    fn parse_packet() {
        //let mut data = Bytes::from_static(&[
        //    0x06, 0x00, 0x00, 0x00, 0x0f,
        //    0x35, 0x03, 0x19, 0x12, 0x00,
        //    0x03, 0xf0, 0x35, 0x05, 0x0a,
        //    0x00, 0x00, 0xff, 0xff, 0x00]);

        let mut data = Bytes::from_static(&[
            0x06, 0x00, 0x00, 0x00, 0x0f,
            0x35, 0x03, 0x19, 0x01, 0x00,
            0x03, 0xf0, 0x35, 0x05, 0x0a,
            0x00, 0x00, 0xff, 0xff, 0x00]);

        let header: SdpHeader = data.read().unwrap();
        println!("{:#?}", header);
        let service_search_patterns: DataElement = data.read().unwrap();
        let max_attr_len: u16 = data.read_be().unwrap();
        let attributes: DataElement = data.read().unwrap();
        let cont: ContinuationState = data.read_be().unwrap();
        data.advance(cont as usize);
        data.finish().unwrap();
        let sdp = SdpServer::default();
        let records = sdp.collect_records(service_search_patterns, attributes).unwrap();
        println!("{:?}", records);
        let mut buffer = BytesMut::new();
        buffer.write(&records);
        println!("{:x?}", buffer.chunk());
        let expected = &[
            0x35, 0x3c, 0x35, 0x3a, 0x09, 0x00, 0x00, 0x0a, 0x00, 0x01, 0x00, 0x01, 0x09, 0x00, 0x01, 0x35,
            0x03, 0x19, 0x11, 0x0b, 0x09, 0x00, 0x04, 0x35, 0x10, 0x35, 0x06, 0x19, 0x01, 0x00, 0x09, 0x00,
            0x19, 0x35, 0x06, 0x19, 0x00, 0x19, 0x09, 0x01, 0x03, 0x09, 0x00, 0x05, 0x35, 0x03, 0x19, 0x10,
            0x02, 0x09, 0x00, 0x09, 0x35, 0x08, 0x35, 0x06, 0x19, 0x11, 0x0d, 0x09, 0x01, 0x03
        ];
        assert_eq!(buffer.chunk(), expected);
    }

}

 */