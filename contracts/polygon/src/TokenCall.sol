// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// ERC-20 calls that fail loudly: the call must succeed, and a returned value must be true. A token that returns nothing is
/// accepted, as many do. The escrow's constructor refuses a token with no code, and code can't disappear afterwards (EIP-6780),
/// so a call to an empty address, which would succeed silently, can't happen here.
library TokenCall {
    error TokenCallFailed();

    function transfer(address token, address to, uint256 amount) internal {
        _call(token, abi.encodeWithSelector(0xa9059cbb, to, amount));
    }

    function transferFrom(address token, address from, address to, uint256 amount) internal {
        _call(token, abi.encodeWithSelector(0x23b872dd, from, to, amount));
    }

    function balanceOf(address token, address who) internal view returns (uint256) {
        (bool ok, bytes memory data) = token.staticcall(abi.encodeWithSelector(0x70a08231, who));
        if (!ok || data.length < 32) revert TokenCallFailed();
        return abi.decode(data, (uint256));
    }

    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory ret) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (ret.length != 0 && (ret.length != 32 || abi.decode(ret, (uint256)) != 1)) revert TokenCallFailed();
    }
}
