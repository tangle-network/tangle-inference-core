#!/usr/bin/env node
// Internal implementation for blueprint registration and its release workflow.
import { createHash, randomUUID } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { open, readFile, writeFile, mkdir, copyFile } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';

async function binaryHash(path) {
  const file = await open(path, 'r');
  const header = Buffer.alloc(20);
  try { await file.read(header, 0, header.length, 0); } finally { await file.close(); }
  if (!header.subarray(0, 4).equals(Buffer.from([0x7f, 69, 76, 70]))
      || header[4] !== 2 || header[5] !== 1 || header.readUInt16LE(18) !== 62) {
    throw new Error('Expected an actual Linux amd64 ELF operator binary');
  }
  const digest = createHash('sha256');
  for await (const chunk of createReadStream(path)) digest.update(chunk);
  return digest.digest('hex');
}

function sourceOf(definition) {
  const source = definition.sources?.[0];
  if (source?.kind !== 'Native' || !source.native) throw new Error('Native source required');
  return source;
}

function checkDefinition(definition) {
  const source = sourceOf(definition);
  const binaries = source.native.binaries ?? source.native.github?.binaries;
  if (!binaries?.length || binaries.some(binary => !/^[a-fA-F0-9]{64}$/.test(binary.sha256)
      || /^0+$/.test(binary.sha256) || binary.arch !== 'amd64' || binary.os !== 'linux')) {
    throw new Error('Definition requires real Linux amd64 binary digests from the release workflow');
  }
  if (!['Github', 'None'].includes(source.native.fetcher)) throw new Error('Unsupported native fetcher');
  if (typeof definition.metadata?.profiling_data !== 'string') throw new Error('profiling_data required');
  try { JSON.parse(definition.metadata.profiling_data); } catch { throw new Error('Invalid profiling_data JSON'); }
  return source;
}

async function localAnvil() {
  const url = new URL(process.env.RPC_URL);
  if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) {
    throw new Error('Local binary registration requires a loopback Anvil RPC');
  }
  const response = await fetch(url, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_chainId', params: [] }),
    signal: AbortSignal.timeout(15_000),
  });
  if (!response.ok || (await response.json()).result !== '0x7a69') {
    throw new Error('Local binary registration requires Anvil chain 31337');
  }
}

async function jsonFrom(location) {
  if (/^https?:/.test(location)) {
    const url = new URL(location);
    if (url.protocol !== 'https:') await localAnvil();
    let response;
    try { response = await fetch(url, { signal: AbortSignal.timeout(30_000) }); }
    catch { throw new Error('Release definition fetch failed'); }
    if (!response.ok) throw new Error(`Release definition unavailable (HTTP ${response.status})`);
    try { return await response.json(); } catch { throw new Error('Invalid release definition JSON'); }
  }
  const contents = await readFile(location, 'utf8');
  try { return JSON.parse(contents); } catch { throw new Error('Invalid blueprint definition JSON'); }
}

async function prepare(templatePath, root) {
  const template = await jsonFrom(templatePath);
  const source = sourceOf(template);
  let definition;
  if (process.env.BLUEPRINT_BINARY) {
    await localAnvil();
    definition = structuredClone(template);
    const binary = resolve(process.env.BLUEPRINT_BINARY);
    const descriptor = { name: basename(binary), arch: 'amd64', os: 'linux', sha256: await binaryHash(binary) };
    definition.sources[0].native = {
      fetcher: 'None', artifact_uri: pathToFileURL(binary).href,
      entrypoint: binary, binaries: [descriptor],
    };
  } else if (process.env.BLUEPRINT_DEFINITION) {
    definition = await jsonFrom(process.env.BLUEPRINT_DEFINITION);
  } else {
    const { owner, repo } = source.native.github ?? {};
    if (![owner, repo].every(value => typeof value === 'string' && /^[\w.-]+$/.test(value))) {
      throw new Error('GitHub release owner and repository required');
    }
    definition = await jsonFrom(`https://github.com/${owner}/${repo}/releases/latest/download/blueprint-definition.json`);
  }
  const prepared = checkDefinition(definition);
  if (prepared.native.fetcher === 'None') await localAnvil();
  if (definition.metadata.profiling_data !== template.metadata.profiling_data
      || definition.metadata.code_repository !== template.metadata.code_repository) {
    throw new Error('Prepared release definition does not match this blueprint and execution profile');
  }
  const cache = join(resolve(root), 'contracts', 'cache');
  await mkdir(cache, { recursive: true });
  const path = join(cache, `blueprint-definition-${randomUUID()}.json`);
  await writeFile(path, `${JSON.stringify(definition, null, 2)}\n`, { mode: 0o600, flag: 'wx' });
  process.stdout.write(`${path}\n`);
}

async function packageRelease(args) {
  const options = { binaries: [] };
  for (let i = 0; i < args.length; i += 2) {
    if (!args[i + 1]) throw new Error('Packaging options require values');
    if (args[i] === '--binary') options.binaries.push(resolve(args[i + 1]));
    else if (['--definition', '--output-dir', '--tag'].includes(args[i])) options[args[i].slice(2)] = args[i + 1];
    else throw new Error(`Unknown packaging option: ${args[i]}`);
  }
  if (!options.definition || !options['output-dir'] || !options.tag || !options.binaries.length) {
    throw new Error('Packaging requires definition, output-dir, tag and binary');
  }
  const definition = await jsonFrom(options.definition);
  const source = sourceOf(definition);
  const output = resolve(options['output-dir']);
  const target = process.env.TARGET ?? 'x86_64-unknown-linux-gnu';
  if (target !== 'x86_64-unknown-linux-gnu') throw new Error('Linux amd64 release target required');
  await mkdir(output, { recursive: true });
  const manifest = { artifacts: {} };
  const descriptors = new Map();
  for (const path of options.binaries) {
    const name = basename(path);
    if (!/^[\w.-]+$/.test(name)) throw new Error('Invalid operator binary name');
    const sha256 = await binaryHash(path);
    descriptors.set(name, { name, arch: 'amd64', os: 'linux', sha256 });
    const archive = `${name}-linux-amd64.tar.xz`;
    execFileSync('tar', ['-cJf', join(output, archive), '-C', dirname(path), name]);
    const digest = createHash('sha256');
    for await (const chunk of createReadStream(join(output, archive))) digest.update(chunk);
    const archiveHash = digest.digest('hex');
    const legacy = `${name}-${target}.tar.xz`;
    await copyFile(join(output, archive), join(output, legacy));
    for (const item of [archive, legacy]) await writeFile(join(output, `${item}.sha256`), `${archiveHash}  ${item}\n`);
    await writeFile(join(output, `${name}.sha256`), `${sha256}  ${name}\n`);
    await writeFile(join(output, `${name}-${target}.bin.sha256`), `${sha256}\n`);
    manifest.artifacts[archive] = {
      kind: 'executable-zip', target_triples: [target],
      assets: [{ kind: 'executable', name, path: name }],
    };
  }
  const name = source.testing?.cargo_bin ?? basename(source.native.entrypoint);
  const binary = descriptors.get(name);
  if (!binary) throw new Error('Release omitted the blueprint operator binary');
  source.native.fetcher = 'Github';
  source.native.entrypoint = `./${name}`;
  source.native.github.tag = options.tag;
  source.native.github.binaries = [binary];
  source.native.binaries = [binary];
  checkDefinition(definition);
  await writeFile(join(output, 'dist-manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  await writeFile(join(output, 'blueprint-definition.json'), `${JSON.stringify(definition, null, 2)}\n`);
}

try {
  if (process.argv[2] === '--package') await packageRelease(process.argv.slice(3));
  else if (process.argv.length === 4) await prepare(process.argv[2], process.argv[3]);
  else throw new Error('Registration requires the blueprint definition and repository root');
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
