// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.26;

/// Minimal EIP-3009 token for the fully offline dev loop: exactly the
/// surface the x402 payment path touches — transferWithAuthorization (both
/// the v,r,s and bytes-signature forms, since USDC ships both and the
/// facilitator may call either), balanceOf, and an open mint for funding
/// test accounts. The EIP-712 domain mirrors USDC on Base in its name,
/// version, and chain-id fields ("USD Coin", "2", 8453) so gateway and
/// facilitator defaults hold; the verifyingContract field necessarily
/// differs (this is a different address), so signatures are not portable
/// between the forked and offline chains.
///
/// Not a real token: mint is unrestricted, there is no approve/transferFrom,
/// no pausing, no blacklist, and _recover accepts anything ecrecover does
/// (no low-s enforcement — replay is keyed by (from, nonce) per EIP-3009,
/// so this is laxer than USDC's signature policy but not double-spendable).
/// Test value only.
///
/// The committed MockUSDC.bin is the creation bytecode of exactly this
/// source, compiled with solc 0.8.26 (see contracts/README note in
/// docker-compose.offline.yml); deploys happen with `cast send --create`
/// so no compiler is needed at stack-up time.
contract MockUSDC {
    string public constant name = "USD Coin";
    string public constant version = "2";
    string public constant symbol = "mUSDC";
    uint8 public constant decimals = 6;

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(bytes32 => bool)) public authorizationState;

    bytes32 public constant TRANSFER_WITH_AUTHORIZATION_TYPEHASH = keccak256(
        "TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)"
    );

    bytes32 public immutable DOMAIN_SEPARATOR;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event AuthorizationUsed(address indexed authorizer, bytes32 indexed nonce);

    constructor() {
        DOMAIN_SEPARATOR = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
                ),
                keccak256(bytes(name)),
                keccak256(bytes(version)),
                block.chainid,
                address(this)
            )
        );
    }

    function mint(address to, uint256 value) external {
        totalSupply += value;
        balanceOf[to] += value;
        emit Transfer(address(0), to, value);
    }

    function transferWithAuthorization(
        address from,
        address to,
        uint256 value,
        uint256 validAfter,
        uint256 validBefore,
        bytes32 nonce,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external {
        _transferWithAuthorization(
            from, to, value, validAfter, validBefore, nonce, abi.encodePacked(r, s, v)
        );
    }

    function transferWithAuthorization(
        address from,
        address to,
        uint256 value,
        uint256 validAfter,
        uint256 validBefore,
        bytes32 nonce,
        bytes memory signature
    ) external {
        _transferWithAuthorization(from, to, value, validAfter, validBefore, nonce, signature);
    }

    function _transferWithAuthorization(
        address from,
        address to,
        uint256 value,
        uint256 validAfter,
        uint256 validBefore,
        bytes32 nonce,
        bytes memory signature
    ) internal {
        require(block.timestamp > validAfter, "authorization is not yet valid");
        require(block.timestamp < validBefore, "authorization is expired");
        require(!authorizationState[from][nonce], "authorization is used");

        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR,
                keccak256(
                    abi.encode(
                        TRANSFER_WITH_AUTHORIZATION_TYPEHASH,
                        from,
                        to,
                        value,
                        validAfter,
                        validBefore,
                        nonce
                    )
                )
            )
        );
        require(_recover(digest, signature) == from, "invalid signature");

        authorizationState[from][nonce] = true;
        emit AuthorizationUsed(from, nonce);

        require(balanceOf[from] >= value, "transfer amount exceeds balance");
        unchecked {
            balanceOf[from] -= value;
        }
        balanceOf[to] += value;
        emit Transfer(from, to, value);
    }

    function _recover(bytes32 digest, bytes memory signature)
        internal
        pure
        returns (address)
    {
        require(signature.length == 65, "invalid signature length");
        bytes32 r;
        bytes32 s;
        uint8 v;
        assembly {
            r := mload(add(signature, 32))
            s := mload(add(signature, 64))
            v := byte(0, mload(add(signature, 96)))
        }
        address signer = ecrecover(digest, v, r, s);
        require(signer != address(0), "invalid signature");
        return signer;
    }
}
