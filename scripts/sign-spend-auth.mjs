#!/usr/bin/env node
// Private key on stdin. All other options retain the inference blueprint CLI.
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

try {
  const args = process.argv.slice(2);
  if (args.length % 2) throw new Error('Options require values');
  const values = Object.fromEntries(Array.from({ length: args.length / 2 }, (_, i) => [args[2 * i].replace(/^--/, '').replaceAll('-', '_'), args[2 * i + 1]]));
  const allowed = ['verifying_contract', 'chain_id', 'commitment', 'service_id', 'job_index', 'amount', 'operator', 'nonce', 'expiry'];
  if (Object.keys(values).some(key => !allowed.includes(key))) throw new Error('Unknown option; provide the signing key on stdin');
  for (const key of allowed) if (!values[key]) throw new Error(`Missing --${key.replaceAll('_', '-')}`);
  // Rust parses integer strings without losing precision to JS Numbers.
  const json = JSON.stringify({
    private_key: readFileSync(0, 'utf8').trim(),
    verifying_contract: values.verifying_contract,
    chain_id: values.chain_id,
    auth: { ...Object.fromEntries(allowed.filter(k => !['chain_id', 'verifying_contract'].includes(k)).map(k => [{ service_id: 'serviceId', job_index: 'jobIndex' }[k] ?? k, values[k]])), signature: '0x' },
  }).replace(/"chain_id":"(\d+)"/, '"chain_id":$1');
  const result = spawnSync('cargo', ['run', '--quiet', '--locked', '--manifest-path', fileURLToPath(new URL('../Cargo.toml', import.meta.url)), '--example', 'sign_spend_auth', '--'], { input: json, encoding: 'utf8' });
  if (result.error || result.status !== 0) throw new Error('Signing failed; check Rust toolchain and input');
  process.stdout.write(result.stdout);
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
