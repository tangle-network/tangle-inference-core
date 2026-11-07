"""Register a blueprint (deploy/definition.json; a Forge script printing DEPLOY_MANAGER=<address>) on local Anvil with the released cargo-tangle, then check what the chain recorded."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import shutil
import subprocess
import tempfile
import time
import urllib.request

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--blueprint', type=Path, required=True, help='blueprint checkout')
p.add_argument('--deploy', default='script/Deploy.s.sol:Deploy', help='manager Forge script')
p.add_argument('--binary', type=Path, required=True)
p.add_argument('--cli', default=os.environ.get('CARGO_TANGLE_BIN', 'cargo-tangle'))
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
repo = a.blueprint.resolve()
contracts = repo / 'contracts'
# Forge compiles scripts inside the blueprint's project, so the core fixture is copied there for the run.
fixture = contracts / 'test' / 'CoreNativeRegistryFixture.s.sol'
binary = a.binary.resolve()
assert binary.read_bytes()[:4] == b'\x7fELF', 'supply the actual built Linux blueprint binary'
binary_sha = hashlib.sha256(binary.read_bytes()).hexdigest()


def run(argv, *, env=None, check=True):
    r = subprocess.run(argv, cwd=contracts, env=env, text=True, capture_output=True, timeout=180)
    if check and r.returncode:
        raise AssertionError(f'{argv[0]} failed ({r.returncode}): {r.stdout}\n{r.stderr}')
    return r


def rpc(method, params, expect_error=False):
    data = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params}).encode()
    request = urllib.request.Request(url, data=data, headers={'Content-Type': 'application/json'})
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=5) as response:
        value = json.load(response)
    if expect_error:
        assert 'error' in value, 'RPC still signs for the fixture account'
        return value['error']
    assert 'error' not in value, value
    return value['result']


def keypair(root):
    result = run([a.cli, 'key', 'generate', '--key-type', 'ecdsa', '--output', str(root)])
    public = bytes.fromhex(re.search(r'Public key: ([0-9a-fA-F]+)', result.stdout)[1])
    encoded = bytes.fromhex('3036301006072a8648ce3d020106052b8104000a032200') + public
    uncompressed = subprocess.run(['openssl', 'ec', '-pubin', '-inform', 'DER', '-conv_form', 'uncompressed', '-outform', 'DER'],
                                  input=encoded, capture_output=True, check=True).stdout[-65:]
    assert uncompressed[0] == 4
    digest = run(['cast', 'keccak', '0x' + uncompressed[1:].hex()]).stdout.strip()
    address = '0x' + digest[-40:]
    rpc('anvil_setBalance', [address, hex(100 * 10**18)])
    return address


def revert(error, *addresses):
    """Hex of the manager's custom error as the registry relays it inside its own revert."""
    return run(['cast', 'sig', error]).stdout.strip()[2:] + ''.join(x[2:].lower().rjust(64, '0') for x in addresses)


def read(address, signature):
    return run(['cast', 'call', address, signature, '--rpc-url', url]).stdout.strip()


run(['forge', 'soldeer', 'update', '--config-location', 'foundry', '--quiet'])
shutil.copyfile(Path(__file__).resolve().with_name('NativeRegistryFixture.s.sol'), fixture)
# Keep run state in the checkout: host /tmp reapers removed it mid-run on loaded gate hosts.
(contracts / 'cache').mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix='native-registration-', dir=contracts / 'cache') as temp:
    temp = Path(temp)
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    url = f'http://127.0.0.1:{port}'
    log = (temp / 'anvil.log').open('w')
    anvil = subprocess.Popen(['anvil', '--host', '127.0.0.1', '--port', str(port), '--chain-id', '31337', '--silent'],
                             stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 15
        while True:
            try:
                assert rpc('eth_chainId', []) == '0x7a69'
                break
            except Exception:
                if anvil.poll() is not None or time.monotonic() >= deadline:
                    raise AssertionError('local Anvil did not start')
                time.sleep(.05)
        creator = keypair(temp / 'creator')
        attacker = keypair(temp / 'attacker')
        rpc('anvil_impersonateAccount', [creator])
        env = os.environ.copy()
        env.pop('PRIVATE_KEY', None)
        env.update(NATIVE_TEST_OWNER=creator)
        result = run(['forge', 'script', 'test/CoreNativeRegistryFixture.s.sol:NativeRegistryFixture', '--rpc-url', url,
                      '--unlocked', '--sender', creator, '--broadcast'], env=env)
        registry = re.search(r'NATIVE_TEST_REGISTRY=(0x[0-9a-fA-F]{40})', result.stdout)[1]
        env.update(TANGLE_CORE=registry, PAYMENT_TOKEN='0x0000000000000000000000000000000000001001')
        result = run(['forge', 'script', a.deploy, '--rpc-url', url, '--unlocked',
                      '--sender', creator, '--broadcast'], env=env)
        manager = re.search(r'DEPLOY_MANAGER=(0x[0-9a-fA-F]{40})', result.stdout)[1]
        for address in [creator, attacker]:
            rpc('anvil_stopImpersonatingAccount', [address])
            rpc('eth_sendTransaction', [{'from': address, 'to': address, 'value': '0x0', 'gas': '0x5208'}], expect_error=True)
        assert read(manager, 'blueprintOwner()(address)').lower() == creator.lower()
        assert read(manager, 'tangleCore()(address)').lower() == registry.lower()
        assert int(read(manager, 'blueprintId()(uint64)').split()[0]) == 2**64 - 1
        definition = json.loads((repo / 'deploy/definition.json').read_text())
        definition['manager'] = manager
        source = definition['sources'][0]
        source.update(fetcher='none', artifact_uri=str(binary), binaries=[{
            'name': binary.name, 'arch': 'amd64', 'os': 'linux', 'sha256': binary_sha}])
        source.pop('github', None)
        source['entrypoint'] = './' + binary.name
        definition_path = temp / 'definition.json'
        definition_path.write_text(json.dumps(definition))
        settings = temp / 'settings.env'
        settings.write_text(f'BLUEPRINT_ID=0\nTANGLE_CONTRACT={registry}\nSTAKING_CONTRACT={registry}\nSTATUS_REGISTRY_CONTRACT={registry}\n')
        args = [a.cli, 'blueprint', 'deploy', 'tangle', '--network', 'testnet', '--settings-file', str(settings),
                '--definition', str(definition_path), '--http-rpc-url', url, '--ws-rpc-url', url.replace('http:', 'ws:'),
                '--tangle-contract', registry, '--staking-contract', registry, '--status-registry-contract', registry]
        # The public registry relays the attacker's own signed request to the real manager.
        foreign = run(args + ['--keystore-path', str(temp / 'attacker')], check=False)
        assert foreign.returncode != 0 and revert('OnlyBlueprintOwnerAllowed(address,address)', attacker, creator) in (
            foreign.stdout + foreign.stderr).lower(), foreign
        assert int(read(registry, 'blueprintCount()(uint64)').split()[0]) == 0
        run(args + ['--keystore-path', str(temp / 'creator')])
        assert int(read(registry, 'blueprintCount()(uint64)').split()[0]) == 1
        assert int(read(manager, 'blueprintId()(uint64)').split()[0]) == 0
        assert read(manager, 'blueprintOwner()(address)').lower() == creator.lower()
        # TNT 0.20 event-sources `sources`; the stored hash anchors the recorded definition bytes.
        topic = run(['cast', 'keccak', 'BlueprintDefinitionRecorded(uint64,address,bytes)']).stdout.strip()
        logs = rpc('eth_getLogs', [{'fromBlock': '0x0', 'toBlock': 'latest', 'topics': [topic, '0x' + '00' * 32]}])
        assert len(logs) == 1 and int(logs[0]['topics'][2], 16) == int(creator, 16), logs
        event = bytes.fromhex(logs[0]['data'].removeprefix('0x'))
        wire = event[64:64 + int.from_bytes(event[32:64], 'big')]
        recorded = run(['cast', 'keccak', '0x' + wire.hex()]).stdout.strip()
        assert recorded == run(['cast', 'call', registry, 'blueprintDefinitionHash(uint64)(bytes32)', '0', '--rpc-url', url]).stdout.strip(), 'recorded definition is not the anchored one'
        profile = definition['metadata']['profiling_data']
        metadata_wire = run(['cast', 'call', registry, 'blueprintMetadata(uint64)', '0', '--rpc-url', url]).stdout.strip()
        metadata = run(['cast', 'decode-abi', '--json', 'blueprintMetadata(uint64)((' + ','.join(['string'] * 9) + '),string,bytes32)', metadata_wire]).stdout
        decoded = json.loads(metadata)
        decoded = (decoded['data'] if isinstance(decoded, dict) else decoded)[0]
        a.output.with_suffix('.metadata.json').write_text(json.dumps({'wire': metadata_wire, 'decoded': decoded}, indent=2) + '\n')
        observed_profile = json.loads(decoded[-1]) if isinstance(decoded[-1], str) else decoded[-1]
        assert observed_profile == json.loads(profile), (observed_profile, profile)
        assert bytes.fromhex(binary_sha) in wire, 'registered binary digest changed'
        repeated = run(args + ['--keystore-path', str(temp / 'creator')], check=False)
        assert repeated.returncode != 0 and revert('AlreadyInitialized()') in (repeated.stdout + repeated.stderr).lower(), repeated
        assert int(read(registry, 'blueprintCount()(uint64)').split()[0]) == 1
        report = {'source': run(['git', 'rev-parse', 'HEAD']).stdout.strip(), 'registry': registry,
                  'manager': manager, 'creator': creator, 'blueprint_id': 0, 'profiling_data': observed_profile,
                  'binary_sha256': binary_sha, 'binary_bytes': binary.stat().st_size,
                  'binary_path': str(binary), 'cli_version': run([a.cli, '--version']).stdout.strip(),
                  'cli_sha256': hashlib.sha256(Path(shutil.which(a.cli)).read_bytes()).hexdigest(),
                  'foreign_relay_refused': True, 'repeat_refused': True, 'unsigned_rpc_send_refused_for_both': True,
                  'boundary': 'Released native CLI signing from the SDK keystore, the blueprint manager Forge deployment and the Tangle registry on local Anvil'}
        a.output.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report))
    finally:
        fixture.unlink(missing_ok=True)
        anvil.terminate()
        try:
            anvil.wait(timeout=5)
        except subprocess.TimeoutExpired:
            anvil.kill()
            anvil.wait(timeout=5)
        log.close()
