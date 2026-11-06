import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('../scripts/sign-spend-auth.mjs', import.meta.url));
// Public deterministic Anvil development account.
const key = 'ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80';
const args = ['--verifying-contract', '0x0000000000000000000000000000000000000001', '--chain-id', '31337', '--commitment', `0x${'11'.repeat(32)}`, '--service-id', '9007199254740993', '--job-index', '0', '--amount', '1000000000000000000', '--operator', '0x0000000000000000000000000000000000000002', '--nonce', '9007199254740993', '--expiry', '9999999999'];
const result = spawnSync('node', [script, ...args], { input: key, encoding: 'utf8' });
assert.equal(result.status, 0, result.stderr);
assert(!result.stdout.includes(key));
assert(!result.stderr.includes(key));
const signed = JSON.parse(result.stdout);
const typed = {
  domain: { name: 'ShieldedCredits', version: '1', chainId: '31337', verifyingContract: args[1] },
  primaryType: 'SpendAuthorization',
  types: {
    EIP712Domain: [{ name: 'name', type: 'string' }, { name: 'version', type: 'string' }, { name: 'chainId', type: 'uint256' }, { name: 'verifyingContract', type: 'address' }],
    SpendAuthorization: [{ name: 'commitment', type: 'bytes32' }, { name: 'serviceId', type: 'uint64' }, { name: 'jobIndex', type: 'uint8' }, { name: 'amount', type: 'uint256' }, { name: 'operator', type: 'address' }, { name: 'nonce', type: 'uint256' }, { name: 'expiry', type: 'uint64' }],
  },
  message: { commitment: args[5], serviceId: args[7], jobIndex: args[9], amount: args[11], operator: args[13], nonce: args[15], expiry: args[17] },
};
const typedVerified = spawnSync('cast', ['wallet', 'verify', '--data', '--address', '0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266', JSON.stringify(typed), signed.signature], { encoding: 'utf8' });
assert.equal(typedVerified.status, 0, typedVerified.stderr);
const verified = spawnSync('cast' , ['wallet', 'verify', '--no-hash', '--address', '0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266', signed.digest, signed.signature], { encoding: 'utf8' });
assert.equal(verified.status, 0, verified.stderr);
const forbidden = spawnSync('node', [script, '--private-key', 'not-a-key', ...args], { input: key, encoding: 'utf8' });
assert.equal(forbidden.status, 1);
assert(!forbidden.stdout.includes(key) && !forbidden.stderr.includes(key));
console.log('stdin signing and large integer wire proof passed');
