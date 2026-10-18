#!/usr/bin/env python3
"""Write a downloaded Lenovo recovery (ddrs.py) onto a USB stick from macOS or Linux.

Mirrors USBRecoveryCreator's BurnUSBViewModel: PartitionUSB/CleanMBR -> FormatUSB -> CopyFiles ->
WriteValuetoSector(62, 80) -> ValidateUSBKey -> ApplyPatch.

  python3 build_usb.py selftest
  python3 build_usb.py assets          # fetch Lenovo's tool and extract the two boot files it embeds
  python3 build_usb.py disks           # list USB disks as JSON
  python3 build_usb.py build DISK      # ERASES DISK (macOS: diskN; Linux: sdX, run as root)
  python3 build_usb.py rawfix DISK     # just the boot-sector step (build runs it at the end)
"""
import glob, hashlib, json, os, plistlib, re, shutil, socket, struct, subprocess, sys, tempfile, time, urllib.request, zipfile, zlib
import xml.etree.ElementTree as ET

MAC = sys.platform == 'darwin'
HERE = os.path.dirname(os.path.abspath(__file__))
FILES = os.path.join(HERE, 'work', 'files')
ASSETS = os.path.join(HERE, 'assets')
PATCH_ZIP = os.path.join(ASSETS, 'Patch.zip')       # embedded USBRecoveryCreator.lib.Patch.zip
ZEROSECTOR = os.path.join(ASSETS, 'zerosector.ucm')  # embedded USBRecoveryCreator.lib.zerosector.ucm
TOOL_URL = 'https://tools.dds.lenovo.com/win8/USBRecoveryCreator.exe'
ASSET_SHA = {PATCH_ZIP: 'c3789261c3f8d8d4a5dd8a34e63e8eb46e9776eb9ec0dd48d11281d6177edeea',
             ZEROSECTOR: 'ace0fb7a119a776657a9b761e757cf2780e3ded807e4c3c7701fa5f0e5621ff3'}
ASSET_LEN = {PATCH_ZIP: 5050303, ZEROSECTOR: 512}
MARKER_OFF = (62 - 1) * 512                                 # readWriteSector: (sector - 1) << 9
MARKER = b'StateApp' + bytes([80, 0, 0, 0, 0, 0]) + b'ppAetatS'  # writeStatus at OFFSET2 = 16
SEVENZIP = shutil.which('7zz') or shutil.which('7z')
NOT_USB = 'not a USB/removable disk'
SAFE_MOUNTS = ('/media/', '/run/media/', '/mnt/', '/Volumes/')  # where removable sticks get auto-mounted


def cypher_decode(s):  # Utilities/Cypher.cs
    k = 'k`gybs0' + 'vampjd'
    if not all(c.isascii() and c.isalnum() for c in s):
        return None
    return ''.join(chr(ord(k[ord(c) % len(k)]) + 2 - i % 3) for i, c in enumerate(s))


def read_lines(path):  # File.ReadLines
    lines = re.split(r'\r\n|\n|\r', open(path, encoding='utf-8-sig').read())
    return lines[:-1] if lines and lines[-1] == '' else lines


def write_lines(path, lines):  # File.WriteAllLines on Windows: UTF-8 no BOM, CRLF
    with open(path, 'wb') as f:
        f.write(''.join(l + '\r\n' for l in lines).encode('utf-8'))


def update_aod(lines, module):  # BurnUSBViewModel.UpdateAOD, quirks included
    t = next(l for l in lines if 'Unit(1).SW_Module()=' in l).split(':')[1]
    n = int(t) + 1
    lines = [l.replace(t, str(n)) if 'Unit(1).SW_Module()=' in l else l for l in lines]
    return lines + ['  SW_Module(%d).CRI_File=%s.CRI' % (n, module)]


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for b in iter(lambda: f.read(1 << 22), b''):
            h.update(b)
    return h.hexdigest()


def run(*cmd, check=True):
    print('$ ' + ' '.join(cmd), flush=True)
    return subprocess.run(cmd, check=check).returncode


def unbundle(exe, want):  # one file out of a .NET single-file bundle (format v6)
    d = open(exe, 'rb').read()
    i = d.find(bytes.fromhex('8b1202b96a612038727b930214d7a03213f5b9e6efae3318ee3b2dce24b36aae'))
    if i < 0:
        sys.exit('%s is not the .NET build of USBRecoveryCreator this tool knows' % exe)
    p = struct.unpack_from('<q', d, i - 8)[0]

    def rstr(p):
        n = s = 0
        while True:
            b = d[p]; p += 1; n |= (b & 0x7f) << s; s += 7
            if b < 0x80:
                return d[p:p + n].decode(), p + n
    major, _, count = struct.unpack_from('<IIi', d, p)
    _, p = rstr(p + 12)
    p += 40 if major >= 2 else 0
    for _ in range(count):
        off, size, csize = struct.unpack_from('<qqq', d, p)
        name, p = rstr(p + 25)
        if name == want:
            blob = d[off:off + (csize or size)]
            return zlib.decompress(blob, -15) if csize else blob
    sys.exit('%s not found in %s' % (want, exe))


def ensure_assets():
    """Patch.zip + zerosector.ucm are Lenovo's, so they are not shipped: pull them out of Lenovo's own tool."""
    if all(os.path.exists(a) and sha256(a) == h for a, h in ASSET_SHA.items()):
        return
    exe = os.path.join(HERE, 'USBRecoveryCreator.exe')
    if not os.path.exists(exe):
        exe = os.path.join(HERE, 'work', 'USBRecoveryCreator.exe')
        os.makedirs(os.path.dirname(exe), exist_ok=True)
        print('Downloading Lenovo USB Recovery Creator (75 MB) from ' + TOOL_URL, flush=True)
        with urllib.request.urlopen(TOOL_URL, timeout=60) as r, open(exe + '.tmp', 'wb') as f:
            shutil.copyfileobj(r, f)
        os.rename(exe + '.tmp', exe)
    dll = unbundle(exe, 'USBRecoveryCreator.dll')
    os.makedirs(ASSETS, exist_ok=True)
    for a, h in ASSET_SHA.items():  # managed resources are stored as <uint32 length><bytes>
        tag, at = ASSET_LEN[a].to_bytes(4, 'little'), -1
        while True:
            at = dll.find(tag, at + 1)
            if at < 0:
                sys.exit('%s not found inside Lenovo\'s tool (new version?) - please open an issue' % os.path.basename(a))
            blob = dll[at + 4:at + 4 + ASSET_LEN[a]]
            if hashlib.sha256(blob).hexdigest() == h:
                open(a, 'wb').write(blob)
                break
    print('Boot files extracted from Lenovo\'s tool into ' + ASSETS)


# ---- disks -------------------------------------------------------------------------------------

def disk_info(dev):  # macOS
    return plistlib.loads(subprocess.run(['diskutil', 'info', '-plist', dev], check=True, capture_output=True).stdout)


def lsblk(disk=None):  # Linux
    out = subprocess.run(['lsblk', '-J', '-b', '-o', 'NAME,TYPE,SIZE,TRAN,RM,HOTPLUG,MODEL,LOG-SEC,MOUNTPOINT']
                         + (['/dev/' + disk] if disk else []), check=True, capture_output=True, text=True).stdout
    return [d for d in json.loads(out)['blockdevices'] if d['type'] == 'disk']


def truthy(v):  # lsblk prints booleans as true/false or "1"/"0" depending on version
    return v in (True, 1, '1')


def describe(d):
    """Linux lsblk entry -> dict(id, name, size, problem)."""
    mounts = [c.get('mountpoint') for c in [d] + d.get('children', []) if c.get('mountpoint')]
    problem = None
    if not (d.get('tran') == 'usb' or truthy(d.get('rm')) or truthy(d.get('hotplug'))):
        problem = NOT_USB
    elif int(d.get('log-sec') or 512) != 512:
        problem = 'not 512-byte sectors'
    elif any(not m.startswith(SAFE_MOUNTS) for m in mounts):
        problem = 'in use by the system (%s)' % ', '.join(mounts)
    return {'id': d['name'], 'name': (d.get('model') or 'USB disk').strip(), 'size': int(d['size']), 'problem': problem}


def mac_describe(disk):
    i = disk_info(disk)
    problem = None
    if i.get('Internal') or not i.get('WholeDisk'):
        problem = 'internal disk or not a whole disk'
    elif i.get('DeviceBlockSize') != 512:
        problem = 'not 512-byte sectors'
    return {'id': disk, 'name': i.get('MediaName') or 'USB disk', 'size': i.get('TotalSize', 0), 'problem': problem}


def list_disks():
    if MAC:
        out = plistlib.loads(subprocess.run(['diskutil', 'list', '-plist', 'external', 'physical'],
                                            check=True, capture_output=True).stdout)
        return [mac_describe(d) for d in out.get('WholeDisks', [])]
    return [x for x in map(describe, lsblk()) if x['problem'] != NOT_USB]


def check_disk(disk):
    d = mac_describe(disk) if MAC else describe(lsblk(disk)[0])
    if d['problem']:
        sys.exit('%s (%s): %s - refusing.' % (disk, d['name'], d['problem']))
    return d


def unmount(disk):  # a freshly mounted (FSKit/Spotlight) volume stays busy for a few seconds
    if not MAC:
        for p in glob.glob('/dev/%s?*' % disk):
            subprocess.run(['umount', p], capture_output=True)
        return
    for _ in range(10):
        if subprocess.run(['diskutil', 'unmountDisk', disk]).returncode == 0:
            return
        time.sleep(3)
    run('diskutil', 'unmountDisk', 'force', disk)


def erase(disk, label):
    """MBR + one FAT32 partition, mounted. Returns the mount point."""
    if MAC:
        run('diskutil', 'eraseDisk', 'FAT32', label, 'MBRFormat', disk)
        return disk_info(disk + 's1')['MountPoint']
    dev = '/dev/' + disk
    part = dev + ('p1' if disk[-1].isdigit() else '1')
    unmount(disk)
    run('wipefs', '-a', dev)
    print('$ sfdisk ' + dev, flush=True)
    subprocess.run(['sfdisk', '-q', dev], input='label: dos\nstart=2048, type=c, bootable\n', text=True, check=True)
    run('udevadm', 'settle', check=False)
    for _ in range(20):
        if os.path.exists(part):
            break
        time.sleep(0.5)
    subprocess.run(['umount', part], capture_output=True)  # desktop auto-mounters can grab it
    run('mkfs.vfat', '-F', '32', '-n', label, part)
    mnt = tempfile.mkdtemp(prefix='lenovo-usb-')
    run('mount', '-t', 'vfat', part, mnt)
    return mnt


# ---- recovery layout ---------------------------------------------------------------------------

def layout_problem():
    rmf = glob.glob(os.path.join(FILES, '*.RMF'))
    if len(rmf) != 1:
        return 'expected one .RMF file in work/files, found %d' % len(rmf)
    parts = ET.parse(rmf[0]).getroot().findall('recovery/partitions/partition')
    # ponytail: single FAT32 partition only (what known orders use); multi-partition needs the MBR rotation logic
    if len(parts) != 1 or parts[0].get('format').upper() != 'FAT32':
        return 'this order needs %d partition(s) (%s); only a single FAT32 partition is supported so far' % (
            len(parts), ', '.join(p.get('format') for p in parts))
    return None


def manifest():
    problem = layout_problem()
    if problem:
        sys.exit('Unsupported recovery layout: ' + problem)
    rec = ET.parse(glob.glob(os.path.join(FILES, '*.RMF'))[0]).getroot().find('recovery')
    return rec.find('partitions/partition'), rec.findall('files/file'), rec.findall('manualfiles/file')


def local(f):
    return os.path.join(FILES, f.get('name'))


def onstick(mnt, *parts):  # Windows-style '\RECOVERY', '.\BOOT\BCD' -> path on the mounted stick
    return os.path.join(mnt, *(p.replace('\\', '/').strip('/') for p in parts if p))


# ---- boot sector -------------------------------------------------------------------------------

def patch_sectors(fd):
    boot = open(ZEROSECTOR, 'rb').read(440)
    s0 = bytearray(os.pread(fd, 512, 0))
    start = int.from_bytes(s0[454:458], 'little')
    if s0[510:512] != b'\x55\xaa' or s0[450] not in (0x0B, 0x0C) or start * 512 < MARKER_OFF + 512:
        sys.exit('unexpected MBR after erase (type %#x, start LBA %d) - not touching it' % (s0[450], start))
    s0[:440] = boot  # CleanMBR: Windows NT6 MBR bootstrap
    s0[446] = 0x80   # SetPartitions: P0 active
    s0[450] = 0x0B   # SetPartitions: FAT32 -> 0x0B
    os.pwrite(fd, bytes(s0), 0)
    sec = bytearray(os.pread(fd, 512, MARKER_OFF))
    sec[16:16 + len(MARKER)] = MARKER
    os.pwrite(fd, bytes(sec), MARKER_OFF)
    os.fsync(fd)


def authopen(path):
    """macOS: open a raw disk via Apple's authopen (admin password dialog) and receive the fd over a socket."""
    a, b = socket.socketpair()
    p = subprocess.Popen(['/usr/libexec/authopen', '-stdoutpipe', '-o', str(os.O_RDWR), path], stdout=b)
    b.close()
    _, fds, _, _ = socket.recv_fds(a, 64, 1)
    p.wait()
    a.close()
    if not fds:
        sys.exit('Could not open %s for writing (password dialog cancelled, or macOS blocked access to removable '
                 'volumes for the app you started this from: System Settings > Privacy & Security > Files and Folders).' % path)
    return fds[0]


def rawfix(disk):
    check_disk(disk)
    unmount(disk)
    fd = authopen('/dev/r' + disk) if MAC else os.open('/dev/' + disk, os.O_RDWR)
    try:
        patch_sectors(fd)
    finally:
        os.close(fd)
    print('MBR boot code, active flag and StateApp marker written.', flush=True)
    if MAC:
        time.sleep(5)  # macOS re-probes and remounts after the MBR write; Spotlight then re-creates its folders
        mnt = disk_info(disk + 's1').get('MountPoint')
        for d in ('.Spotlight-V100', '.fseventsd') if mnt else ():
            shutil.rmtree(os.path.join(mnt, d), ignore_errors=True)
        unmount(disk)
        run('diskutil', 'eject', disk)
    else:
        run('sync')
    print('DONE - recovery USB is ready. You can unplug it.')


# ---- build -------------------------------------------------------------------------------------

def apply_patch(mnt):  # BurnUSBViewModel.ApplyPatch
    aod = os.path.join(mnt, 'RECOVERY', 'AOD.DAT')
    text = open(aod, encoding='utf-8-sig').read()
    arm = 'WINBARM' in text or 'WIN10ARM' in text
    boot, module = ('bootaa64.efi', 'M9ACB186KP1') if arm else ('BootX64.efi', 'M9CB186KP1')
    with zipfile.ZipFile(PATCH_ZIP) as z:
        def put(src, dst):
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            with z.open('Patch/' + src) as i, open(dst, 'wb') as o:
                shutil.copyfileobj(i, o)
        put('EFI/Boot/' + boot, os.path.join(mnt, 'EFI', 'boot', boot))
        put('EFI/Microsoft/Boot/SKUSiPolicy.P7b', os.path.join(mnt, 'EFI', 'Microsoft', 'Boot', 'SKUSiPolicy.P7b'))
        for m in (module, 'M9CB211C'):
            for ext in ('.CRI', '.IMZ'):
                put(m + ext, os.path.join(mnt, 'RECOVERY', m + ext))
    lines, backed_up = read_lines(aod), False
    for m in (module, 'M9CB211C'):
        if not any(m + '.CRI' in l for l in lines):
            if not backed_up:
                shutil.copyfile(aod, os.path.join(mnt, 'RECOVERY', 'AOD.ORG'))
                backed_up = True
            lines = update_aod(lines, m)
            write_lines(aod, lines)
    print('Patch applied (%s, %s + M9CB211C).' % (boot, module))


def build(disk):
    if not MAC and os.geteuid() != 0:
        sys.exit('On Linux, build must run as root (the app asks for your password via pkexec).')
    if not SEVENZIP:
        sys.exit('7-Zip not found: install it (macOS: brew install sevenzip, Linux: apt/dnf install 7zip or p7zip)')
    ensure_assets()
    part, files, manual = manifest()
    missing = [f.get('name') for f in files if not os.path.exists(local(f))]
    if missing:
        sys.exit('%d files not downloaded yet (e.g. %s) - run ddrs.py download first' % (len(missing), missing[0]))
    archives = [f for f in files if f.get('copy') == '0']
    unpacked = 0
    for f in archives:  # test password + integrity before erasing anything
        print('Testing %s ...' % f.get('name'), flush=True)
        out = subprocess.run([SEVENZIP, 't', '-slt', '-p' + cypher_decode(f.get('key')), local(f)], capture_output=True, text=True)
        if out.returncode:
            sys.exit('archive test failed for %s:\n%s' % (f.get('name'), out.stdout[-800:] + out.stderr[-800:]))
        out = subprocess.run([SEVENZIP, 'l', '-slt', '-p' + cypher_decode(f.get('key')), local(f)], capture_output=True, text=True, check=True)
        unpacked += sum(int(x) for x in re.findall(r'^Size = (\d+)$', out.stdout, re.M))
    need = sum(os.path.getsize(local(f)) for f in files if f.get('copy') != '0') + unpacked + (64 << 20)
    info = check_disk(disk)
    if info['size'] < need:
        sys.exit('stick too small: %.1f GB, need %.1f GB' % (info['size'] / 1e9, need / 1e9))

    label = part.get('label').replace(' ', '').upper()[:11]  # FormatUSB strips spaces from P0's label
    print('ERASING %s (%s, %.1f GB) -> MBR + FAT32 "%s"' % (disk, info['name'], info['size'] / 1e9, label), flush=True)
    mnt = erase(disk, label)
    open(os.path.join(mnt, '.metadata_never_index'), 'w').close()

    copies = [f for f in files if f.get('copy') != '0']
    for i, f in enumerate(copies, 1):
        dst = onstick(mnt, f.get('copypath') or '\\RECOVERY', f.get('name'))
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        print('\r[copy %d/%d] %-40s' % (i, len(copies), f.get('name')), end='', flush=True)
        shutil.copyfile(local(f), dst)
    print()
    for f in archives:
        print('Extracting %s ...' % f.get('name'), flush=True)
        subprocess.run([SEVENZIP, 'x', '-y', '-bso0', '-bsp0', '-p' + cypher_decode(f.get('key')), '-o' + mnt, local(f)], check=True)
    for m in manual:
        d = onstick(mnt, m.get('copypath'))
        os.makedirs(d, exist_ok=True)
        tag = os.path.join(d, 'os.tag')
        if 'aod' in m.get('name').lower() and m.get('ostag') is not None and not os.path.exists(tag):
            open(tag, 'wb').write(m.get('ostag').encode('utf-8'))
        text = (m.findtext('fcontent') or '').replace('\r\n', '\n').replace('\n', '\r\n')
        open(os.path.join(d, m.get('name')), 'wb').write(text.encode('utf-8'))
    if not MAC:
        run('sync')  # read back from the stick, not from the page cache

    checks = [(onstick(mnt, f.get('copypath') or '\\RECOVERY', f.get('name')), f.get('check')) for f in copies]
    checks += [(onstick(mnt, m.get('copypath'), m.get('name')), m.get('check')) for m in manual]
    checks += [(onstick(mnt, c.get('path').lstrip('.')), c.get('check')) for f in archives for c in f.findall('content') if c.get('check')]
    bad = []
    for i, (path, want) in enumerate(checks, 1):
        print('\r[verify %d/%d] %-40s' % (i, len(checks), os.path.basename(path)), end='', flush=True)
        if sha256(path) != want.lower():
            bad.append(os.path.relpath(path, mnt))
    print()
    if bad:
        sys.exit('VERIFY FAILED for %d files: %s' % (len(bad), bad[:10]))
    apply_patch(mnt)

    for junk in glob.glob(os.path.join(mnt, '**', '._*'), recursive=True):
        os.remove(junk)
    for d in ('.Spotlight-V100', '.fseventsd', '.Trashes'):
        shutil.rmtree(os.path.join(mnt, d), ignore_errors=True)
    os.remove(os.path.join(mnt, '.metadata_never_index'))
    if MAC:
        unmount(disk)
    else:
        run('umount', mnt)
        os.rmdir(mnt)
    print('Files written and verified. Writing the boot sector%s ...' % (' (macOS will ask for your password)' if MAC else ''), flush=True)
    rawfix(disk)


def selftest():
    assert cypher_decode('AAA') == 'mlk' and cypher_decode('a-b') is None
    aod = ['[Unit]', 'Unit(1).SW_Module()=1:107', '  SW_Module(107).CRI_File=EMRPEX2511009B.CRI']
    aod = update_aod(update_aod(aod, 'M9CB186KP1'), 'M9CB211C')
    assert aod == ['[Unit]', 'Unit(1).SW_Module()=1:109', '  SW_Module(107).CRI_File=EMRPEX2511009B.CRI',
                   '  SW_Module(108).CRI_File=M9CB186KP1.CRI', '  SW_Module(109).CRI_File=M9CB211C.CRI'], aod
    assert len(MARKER) == 22 and MARKER_OFF == 31232
    usb = {'name': 'sdb', 'type': 'disk', 'size': 64e9, 'tran': 'usb', 'rm': True, 'log-sec': 512, 'model': 'Cruzer ',
           'children': [{'name': 'sdb1', 'mountpoint': '/media/me/STICK'}]}
    assert describe(usb) == {'id': 'sdb', 'name': 'Cruzer', 'size': 64000000000, 'problem': None}
    assert 'in use' in describe(dict(usb, children=[{'mountpoint': '/'}]))['problem']
    assert 'not a USB' in describe(dict(usb, tran='nvme', rm='0', hotplug='0'))['problem']
    if os.path.exists(ZEROSECTOR):  # MBR patch on a fake 1 MiB disk laid out like a fresh eraseDisk/sfdisk
        with tempfile.TemporaryFile() as t:
            img = bytearray(1 << 20)
            img[450], img[454:458], img[510:512] = 0x0C, (2048).to_bytes(4, 'little'), b'\x55\xaa'
            t.write(img); t.flush()
            patch_sectors(t.fileno())
            t.seek(0); out = t.read()
        assert out[:440] == open(ZEROSECTOR, 'rb').read(440) and out[446] == 0x80 and out[450] == 0x0B
        assert out[MARKER_OFF + 16:MARKER_OFF + 38] == MARKER and out[454:458] == img[454:458]
    print('selftest ok')


if __name__ == '__main__':
    a = sys.argv[1:]
    disk_ok = len(a) == 2 and re.fullmatch(r'disk\d+' if MAC else r'[a-z][a-z0-9]*', a[1])
    if a == ['selftest']:
        selftest()
    elif a == ['assets']:
        ensure_assets()
    elif a == ['disks']:
        print(json.dumps(list_disks(), indent=1))
    elif disk_ok and a[0] in ('build', 'rawfix'):
        if not MAC and a[0] == 'rawfix' and os.geteuid() != 0:
            sys.exit('rawfix must run as root on Linux')
        (build if a[0] == 'build' else rawfix)(a[1])
    else:
        sys.exit(__doc__)
