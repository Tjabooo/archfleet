"""Set up SSH aliases for the machines in ~/.config/archfleet/hosts.json, and check them.

    python3 setup_ssh.py            write the aliases + make a key, then say how to authorise it
    python3 setup_ssh.py --copy     also run ssh-copy-id for each Linux machine (asks its password once)
    python3 setup_ssh.py --check    log in to every other machine and round-trip a file over SFTP

Run it on each machine (Linux or Windows). It never touches your other SSH settings: the aliases go in
~/.ssh/config.d/archfleet.conf, included from the top of ~/.ssh/config, and use their own key
~/.ssh/id_ed25519_archfleet (no passphrase, so Archfleet can log in unattended; only public keys
ever leave the machine).
"""
import datetime
import getpass
import hashlib
import shutil
import socket
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from backend import CONFIG, NODES  # noqa: E402

SSH_DIR = Path.home() / '.ssh'
KEY = SSH_DIR / 'id_ed25519_archfleet'
INCLUDE_FILE = SSH_DIR / 'config.d' / 'archfleet.conf'
INCLUDE = 'Include ~/.ssh/config.d/archfleet.conf'
WINDOWS = sys.platform == 'win32'
OPENSSH = Path(r'C:\Windows\System32\OpenSSH')


def tool(name):
    """Windows' own OpenSSH tools (Git Bash's ssh reads a different config)."""
    if WINDOWS and (OPENSSH / f'{name}.exe').exists():
        return str(OPENSSH / f'{name}.exe')
    return shutil.which(name) or name


def is_local(node):
    return node['hostname'].lower() == socket.gethostname().lower()


def make_key():
    SSH_DIR.mkdir(mode=0o700, exist_ok=True)
    if not KEY.exists():
        comment = f'{getpass.getuser()}@{socket.gethostname()}:archfleet'
        subprocess.run([tool('ssh-keygen'), '-q', '-t', 'ed25519', '-N', '', '-f', str(KEY), '-C', comment], check=True)
    if not WINDOWS:
        KEY.chmod(0o600)
    return KEY.with_suffix('.pub').read_text().strip()


def write_config(nodes):
    INCLUDE_FILE.parent.mkdir(mode=0o700, exist_ok=True)
    blocks = ['# Written by Archfleet setup_ssh.py from ' + str(CONFIG) + '. Re-run it after editing hosts.json.\n']
    for node in nodes:
        names = ' '.join(dict.fromkeys([node['alias'], node['hostname']]))
        lines = [f'Host {names}', f"    HostName {node['address'] or node['hostname']}"]
        if node.get('user'):
            lines.append(f"    User {node['user']}")
        lines += ['    IdentityFile ~/.ssh/id_ed25519_archfleet', '    IdentitiesOnly yes',
                  '    StrictHostKeyChecking accept-new', '    ForwardAgent no', '    ConnectTimeout 5',
                  '    ServerAliveInterval 30', '    ServerAliveCountMax 3']
        blocks.append('\n'.join(lines) + '\n')
    INCLUDE_FILE.write_text('\n'.join(blocks))
    config = SSH_DIR / 'config'
    old = config.read_text() if config.exists() else ''
    if INCLUDE not in old.splitlines():
        if old:
            stamp = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
            config.with_name('config.backup-' + stamp).write_text(old)
        # Include must come before any Host block, or it only applies inside that block.
        config.write_text(INCLUDE + '\n\n' + old)
    if not WINDOWS:
        for path in (INCLUDE_FILE, config):
            path.chmod(0o600)


def authorise_hints(nodes, public_key, copy):
    pub = KEY.with_suffix('.pub')
    print(f'\nThis machine\'s Archfleet public key ({pub}):\n  {public_key}\n')
    for node in nodes:
        if node['os'] == 'linux':
            command = [tool('ssh-copy-id'), '-i', str(pub), node['alias']]
            if copy and shutil.which('ssh-copy-id'):
                print(f"--> {node['alias']}: ssh-copy-id (enter {node['user'] or 'your'} password on {node['hostname']} once)")
                subprocess.run(command)
            else:
                print(f"  {node['alias']:<16} ssh-copy-id -i {pub} {node['alias']}")
        else:
            print(f"  {node['alias']:<16} on {node['hostname']}, in an admin PowerShell, from the Archfleet folder:\n"
                  f"                   .\\windows\\setup-ssh-server.ps1 -PublicKeyFile <a copy of {pub.name}>")


def check(nodes):
    failed = False
    for node in nodes:
        result, remote = 'ok', 'archfleet-ssh-test-' + uuid.uuid4().hex + '.tmp'
        local = [Path(tempfile.mkstemp(prefix='archfleet-')[1]) for _ in range(2)]
        try:
            who = subprocess.run([tool('ssh'), '-o', 'BatchMode=yes', node['alias'], 'whoami'], capture_output=True, text=True, timeout=15)
            if who.returncode:
                raise RuntimeError(who.stderr.strip() or 'ssh failed')
            if node.get('user') and who.stdout.strip().split('\\')[-1].lower() != node['user'].lower():
                raise RuntimeError('logged in as ' + who.stdout.strip() + ', expected ' + node['user'])
            payload = ('Archfleet connection test ' + remote).encode()
            local[0].write_bytes(payload)
            batch = f'put "{local[0].as_posix()}" "{remote}"\nget "{remote}" "{local[1].as_posix()}"\nrm "{remote}"\n'
            sftp = subprocess.run([tool('sftp'), '-o', 'BatchMode=yes', '-b', '-', node['alias']], input=batch, capture_output=True, text=True, timeout=25)
            if sftp.returncode:
                raise RuntimeError(sftp.stderr.strip())
            if local[1].read_bytes() != payload:
                raise RuntimeError('SFTP round trip came back different')
            result = 'ok  ssh + sftp  sha256 ' + hashlib.sha256(payload).hexdigest()[:12]
        except Exception as error:
            failed, result = True, f'FAILED: {error}'
        finally:
            for path in local:
                path.unlink(missing_ok=True)
        print(f"  {node['alias']:<16} {result}")
    return failed


def main():
    if not NODES:
        raise SystemExit(f'No machines in {CONFIG} yet: copy hosts.example.json there and edit it first.')
    others = [n for n in NODES if not is_local(n)]
    if len(others) == len(NODES):
        print(f"note: no entry in hosts.json has this machine's hostname ({socket.gethostname()}); treating all as remote.")
    if '--check' in sys.argv:
        raise SystemExit(check(others))
    public_key = make_key()
    write_config(NODES)
    print(f'Wrote SSH aliases for {", ".join(n["alias"] for n in NODES)} to {INCLUDE_FILE}')
    authorise_hints(others, public_key, copy='--copy' in sys.argv)
    print('\nThe Linux machines need sshd running (sudo systemctl enable --now sshd).'
          '\nWhen every machine has run this and been authorised: python3 setup_ssh.py --check')


if __name__ == '__main__':
    main()
