use ethers_core::types::{H256, Address, U256};
use ethers_core::utils::keccak256;
use ethers_signers::{LocalWallet, Signer};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use rmp_serde::Serializer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

#[derive(Serialize)]
pub struct Agent {
    pub source: String,
    pub connectionId: H256,
}

// ─── JSON Wire Types (for the API request body) ────────────────────
// These use camelCase and full field names for the JSON `action` field.

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    pub asset: u32,
    pub is_buy: bool,
    pub limit_px: String,
    pub sz: String,
    pub reduce_only: bool,
    pub order_type: OrderTypeWire,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub enum OrderTypeWire {
    Limit(LimitOrderWire),
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LimitOrderWire {
    pub tif: String,
}

#[derive(Serialize, Clone)]
pub struct ActionWire {
    pub r#type: String, // "order"
    pub orders: Vec<OrderRequest>,
    pub grouping: String,
}

// ─── MsgPack Wire Types (for hash computation) ─────────────────────
// These use abbreviated single-letter keys matching the Python SDK's OrderWire.
// Python SDK: {"a": asset, "b": is_buy, "p": limit_px, "s": sz, "r": reduce_only, "t": order_type}

#[derive(Serialize)]
struct OrderWireMsgPack {
    a: u32,
    b: bool,
    p: String,
    s: String,
    r: bool,
    t: OrderTypeWireMsgPack,
}

#[derive(Serialize)]
struct OrderTypeWireMsgPack {
    limit: LimitOrderWireMsgPack,
}

#[derive(Serialize)]
struct LimitOrderWireMsgPack {
    tif: String,
}

// The action dict for MsgPack: {"type": "order", "orders": [...], "grouping": "na"}
// Note: NO nonce inside the msgpack. Nonce is appended as raw bytes after.
#[derive(Serialize)]
struct ActionMsgPack {
    r#type: String,
    orders: Vec<OrderWireMsgPack>,
    grouping: String,
}

/// Computes the action hash matching the Python SDK's `action_hash()`:
/// ```python
/// def action_hash(action, vault_address, nonce, expires_after):
///     data = msgpack.packb(action)
///     data += nonce.to_bytes(8, "big")
///     if vault_address is None:
///         data += b"\x00"
///     else:
///         data += b"\x01"
///         data += address_to_bytes(vault_address)
///     if expires_after is not None:
///         data += b"\x00"
///         data += expires_after.to_bytes(8, "big")
///     return keccak(data)
/// ```
fn compute_action_hash(
    action: &ActionWire,
    nonce: u64,
    vault_address: Option<&str>,
) -> [u8; 32] {
    // 1. Convert ActionWire to MsgPack format with abbreviated keys
    let msgpack_orders: Vec<OrderWireMsgPack> = action.orders.iter().map(|o| {
        let tif_str = match &o.order_type {
            OrderTypeWire::Limit(l) => l.tif.clone(),
        };
        OrderWireMsgPack {
            a: o.asset,
            b: o.is_buy,
            p: o.limit_px.clone(),
            s: o.sz.clone(),
            r: o.reduce_only,
            t: OrderTypeWireMsgPack {
                limit: LimitOrderWireMsgPack { tif: tif_str },
            },
        }
    }).collect();

    let msgpack_action = ActionMsgPack {
        r#type: action.r#type.clone(),
        orders: msgpack_orders,
        grouping: action.grouping.clone(),
    };

    // 2. Serialize to MsgPack using struct_map for ordered map output
    let mut buf = Vec::new();
    let mut serializer = Serializer::new(&mut buf).with_struct_map();
    msgpack_action.serialize(&mut serializer)
        .expect("MsgPack serialization failed");

    log::info!("MSGPACK HEX: {}", hex::encode(&buf));

    // 3. Append nonce as 8 bytes big-endian (matching Python: nonce.to_bytes(8, "big"))
    buf.extend_from_slice(&nonce.to_be_bytes());

    // 4. Append vault_address marker
    match vault_address {
        None => {
            buf.push(0x00); // No vault
        }
        Some(addr) => {
            buf.push(0x01);
            // Convert hex address to 20 bytes
            let addr_clean = addr.strip_prefix("0x").unwrap_or(addr);
            if let Ok(bytes) = hex::decode(addr_clean) {
                buf.extend_from_slice(&bytes);
            }
        }
    }
    // expires_after is None for regular orders, so we don't append anything

    // 5. keccak256 the whole thing
    keccak256(&buf)
}

pub async fn sign_l1_action(
    private_key: &str,
    action: ActionWire,
    nonce: u64,
) -> Result<(Signature, serde_json::Value), crate::exchange::OrderError> {
    let wallet = LocalWallet::from_str(private_key)
        .map_err(|e| crate::exchange::OrderError::InvalidOrder(e.to_string()))?;

    // 1. Compute action hash (msgpack + nonce + vault_address)
    let action_hash = compute_action_hash(&action, nonce, None);
    let action_hash_h256 = H256::from(action_hash);

    // 2. Construct phantom agent: {"source": "a", "connectionId": action_hash}
    // (source = "a" for mainnet, "b" for testnet)

    // 3. EIP-712 signing of the Agent struct
    // Domain: {name: "Exchange", version: "1", chainId: 1337, verifyingContract: 0x0}
    // Type: Agent(string source, bytes32 connectionId)
    // Message: {source: "a", connectionId: action_hash}

    let domain_separator = ethers_core::types::transaction::eip712::EIP712Domain {
        name: Some("Exchange".to_string()),
        version: Some("1".to_string()),
        chain_id: Some(U256::from(1337)),
        verifying_contract: Some(Address::zero()),
        salt: None,
    };

    let domain_hash = domain_separator.separator();

    let agent_type_hash = keccak256("Agent(string source,bytes32 connectionId)".as_bytes());
    let source_hash = keccak256("a".as_bytes());

    let mut encoded = Vec::new();
    encoded.extend_from_slice(&agent_type_hash);
    encoded.extend_from_slice(&source_hash);
    encoded.extend_from_slice(action_hash_h256.as_bytes());

    let struct_hash = keccak256(&encoded);

    // Final EIP-712 Hash: keccak256("\x19\x01" + domainSeparator + structHash)
    let mut final_payload = Vec::new();
    final_payload.extend_from_slice(&[0x19, 0x01]);
    final_payload.extend_from_slice(&domain_hash);
    final_payload.extend_from_slice(&struct_hash);

    let final_digest = H256::from(keccak256(&final_payload));

    // 4. Sign
    let sig = wallet.sign_hash(final_digest)
        .map_err(|e| crate::exchange::OrderError::InvalidOrder(e.to_string()))?;

    let signature = Signature {
        r: format!("0x{:0>64x}", sig.r),
        s: format!("0x{:0>64x}", sig.s),
        v: sig.v as u8,
    };

    // 5. Build the JSON action with EXACT key order matching Python SDK.
    // CRITICAL: serde_json::json!{} macro alphabetizes keys, but Hyperliquid's server
    // re-msgpacks the JSON request body preserving key order to verify the signature.
    // If our JSON key order differs, the server computes a different hash → wrong recovered address.
    // Python SDK key order:
    //   Outer: type, orders, grouping
    //   Inner OrderWire: a, b, p, s, r, t  (note: s before r!)
    //   OrderType: limit -> {tif: ...}

    let json_orders: Vec<serde_json::Value> = action.orders.iter().map(|o| {
        let tif_str = match &o.order_type {
            OrderTypeWire::Limit(l) => l.tif.clone(),
        };
        // Build order with exact key insertion order: a, b, p, s, r, t
        let mut order_map = serde_json::Map::new();
        order_map.insert("a".to_string(), serde_json::Value::from(o.asset));
        order_map.insert("b".to_string(), serde_json::Value::from(o.is_buy));
        order_map.insert("p".to_string(), serde_json::Value::from(o.limit_px.clone()));
        order_map.insert("s".to_string(), serde_json::Value::from(o.sz.clone()));
        order_map.insert("r".to_string(), serde_json::Value::from(o.reduce_only));

        // Build "t" field: {"limit": {"tif": "Gtc"}}
        let mut tif_map = serde_json::Map::new();
        tif_map.insert("tif".to_string(), serde_json::Value::from(tif_str));
        let mut limit_map = serde_json::Map::new();
        limit_map.insert("limit".to_string(), serde_json::Value::Object(tif_map));
        order_map.insert("t".to_string(), serde_json::Value::Object(limit_map));

        serde_json::Value::Object(order_map)
    }).collect();

    // Build outer action with exact key insertion order: type, orders, grouping
    let mut action_map = serde_json::Map::new();
    action_map.insert("type".to_string(), serde_json::Value::from(action.r#type.clone()));
    action_map.insert("orders".to_string(), serde_json::Value::Array(json_orders));
    action_map.insert("grouping".to_string(), serde_json::Value::from(action.grouping.clone()));

    let action_json = serde_json::Value::Object(action_map);

    Ok((signature, action_json))
}

pub async fn sign_cancel_action(
    private_key: &str,
    asset: u32,
    oid: u64,
    nonce: u64,
) -> Result<(Signature, serde_json::Value), crate::exchange::OrderError> {
    let wallet = LocalWallet::from_str(private_key)
        .map_err(|e| crate::exchange::OrderError::InvalidOrder(e.to_string()))?;

    #[derive(Serialize)]
    struct CancelWireMsgPack {
        a: u32,
        o: u64,
    }

    #[derive(Serialize)]
    struct CancelActionMsgPack {
        r#type: String,
        cancels: Vec<CancelWireMsgPack>,
    }

    let msgpack_action = CancelActionMsgPack {
        r#type: "cancel".to_string(),
        cancels: vec![CancelWireMsgPack { a: asset, o: oid }],
    };

    let mut buf = Vec::new();
    let mut serializer = Serializer::new(&mut buf).with_struct_map();
    msgpack_action.serialize(&mut serializer).expect("MsgPack serialization failed");

    buf.extend_from_slice(&nonce.to_be_bytes());
    buf.push(0x00); // no vault

    let action_hash = keccak256(&buf);
    let action_hash_h256 = H256::from(action_hash);

    let domain_separator = ethers_core::types::transaction::eip712::EIP712Domain {
        name: Some("Exchange".to_string()),
        version: Some("1".to_string()),
        chain_id: Some(U256::from(1337)),
        verifying_contract: Some(Address::zero()),
        salt: None,
    };

    let domain_hash = domain_separator.separator();
    let agent_type_hash = keccak256("Agent(string source,bytes32 connectionId)".as_bytes());
    let source_hash = keccak256("a".as_bytes());

    let mut encoded = Vec::new();
    encoded.extend_from_slice(&agent_type_hash);
    encoded.extend_from_slice(&source_hash);
    encoded.extend_from_slice(action_hash_h256.as_bytes());

    let struct_hash = keccak256(&encoded);

    let mut final_payload = Vec::new();
    final_payload.extend_from_slice(&[0x19, 0x01]);
    final_payload.extend_from_slice(&domain_hash);
    final_payload.extend_from_slice(&struct_hash);

    let final_digest = H256::from(keccak256(&final_payload));

    let sig = wallet.sign_hash(final_digest)
        .map_err(|e| crate::exchange::OrderError::InvalidOrder(e.to_string()))?;

    let signature = Signature {
        r: format!("0x{:0>64x}", sig.r),
        s: format!("0x{:0>64x}", sig.s),
        v: sig.v as u8,
    };

    // JSON format
    let mut cancel_obj = serde_json::Map::new();
    cancel_obj.insert("a".to_string(), serde_json::Value::from(asset));
    cancel_obj.insert("o".to_string(), serde_json::Value::from(oid));

    let mut action_map = serde_json::Map::new();
    action_map.insert("type".to_string(), serde_json::Value::from("cancel"));
    action_map.insert("cancels".to_string(), serde_json::Value::Array(vec![serde_json::Value::Object(cancel_obj)]));

    let action_json = serde_json::Value::Object(action_map);

    Ok((signature, action_json))
}

