#!/usr/bin/env python3
"""Desktop app for making a Lenovo recovery USB on macOS or Linux:  python3 app.py

The UI is index.html in a Chrome/Chromium/Edge/Brave app window, driven over --remote-debugging-pipe.
The same browser runs the Lenovo ID sign-in, where the lenovoid.wust token is intercepted
(what USBRecoveryCreator's WebView2 NavigationStarting handler does). The work itself is ddrs.py and build_usb.py.
"""
import codecs, glob, json, os, queue, re, secrets, shutil, subprocess, sys, tempfile, threading, time
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import build_usb
import ddrs

HERE = os.path.dirname(os.path.abspath(__file__))
DDRS, BUILD, INDEX = (os.path.join(HERE, f) for f in ('ddrs.py', 'build_usb.py', 'index.html'))
PY = [sys.executable, '-I', '-u']
MAC = build_usb.MAC
LOGIN = ('https://passport.lenovo.com/glbwebauthnv6/gateway?lenovoid.action=uilogin&lenovoid.realm=dds1-app.lenovo.com'
         '&lenovoid.lang=en_US&lenovoid.cb=https://passport.lenovo.com/glbwebauthnv6')
TOKEN = secrets.token_urlsafe(24)
HOST = None  # 127.0.0.1:port, checked on every request (DNS rebinding)
UNITS = {'B': 1, 'KB': 1 << 10, 'MB': 1 << 20, 'GB': 1 << 30}


def find_browser():
    if MAC:
        for app in ('Google Chrome', 'Chromium', 'Microsoft Edge', 'Brave Browser'):
            for root in ('/Applications', os.path.expanduser('~/Applications')):
                if os.path.exists('%s/%s.app/Contents/MacOS/%s' % (root, app, app)):
                    return '%s/%s.app/Contents/MacOS/%s' % (root, app, app)
        return None
    names = ('google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'microsoft-edge', 'brave-browser')
    return next(filter(None, map(shutil.which, names)), None)


class Browser:
    """Chrome DevTools Protocol over --remote-debugging-pipe: JSON messages separated by NUL on fds 3 (in) / 4 (out)."""

    def __init__(self, exe, url):
        self.profile = tempfile.mkdtemp(prefix='lenovo-recovery-chrome-')
        to_r, to_w = os.pipe()
        from_r, from_w = os.pipe()
        self.proc = subprocess.Popen(
            ['/bin/sh', '-c', 'exec "$@" 3<&0 4>&1 </dev/null >/dev/null 2>&1', 'sh', exe, '--remote-debugging-pipe',
             '--user-data-dir=' + self.profile, '--no-first-run', '--no-default-browser-check', '--app=' + url],
            stdin=to_r, stdout=from_w)
        os.close(to_r)
        os.close(from_w)
        self.out, self.inp = os.fdopen(to_w, 'wb'), os.fdopen(from_r, 'rb')
        self.lock, self.next_id, self.waiting, self.listeners = threading.Lock(), 0, {}, []
        self.closed, self.ui = threading.Event(), None
        threading.Thread(target=self._read, daemon=True).start()
        self.call('Target.setDiscoverTargets', discover=True)
        for _ in range(100):
            pages = [t for t in self.call('Target.getTargets')['targetInfos'] if t['type'] == 'page']
            if pages:
                self.ui = pages[0]['targetId']
                break
            time.sleep(0.1)

    def _read(self):
        buf = b''
        for chunk in iter(lambda: self.inp.read1(65536), b''):
            *msgs, buf = (buf + chunk).split(b'\0')
            for m in map(json.loads, msgs):
                if 'id' in m:
                    q = self.waiting.pop(m['id'], None)
                    q and q.put(m)
                    continue
                if m['method'] == 'Target.targetDestroyed' and m['params']['targetId'] == self.ui:
                    self.closed.set()
                for q in list(self.listeners):
                    q.put(m)
        self.closed.set()

    def call(self, method, session=None, timeout=30, **params):
        q = queue.Queue(1)
        with self.lock:
            self.next_id += 1
            self.waiting[self.next_id] = q
            msg = dict(id=self.next_id, method=method, params=params, **({'sessionId': session} if session else {}))
            self.out.write(json.dumps(msg).encode() + b'\0')
            self.out.flush()
        r = q.get(timeout=timeout)
        if 'error' in r:
            raise RuntimeError('%s: %s' % (method, r['error'].get('message')))
        return r.get('result', {})

    def quit(self):
        try:
            self.call('Browser.close', timeout=3)
            self.proc.wait(5)
        except Exception:
            self.proc.kill()
        shutil.rmtree(self.profile, ignore_errors=True)


B = None


def capture_wust():
    """Open the Lenovo ID login in a new window and return the first URL carrying lenovoid.wust=."""
    q = queue.Queue()
    B.listeners.append(q)
    try:
        t = B.call('Target.createTarget', url='about:blank', newWindow=True)['targetId']
        s = B.call('Target.attachToTarget', targetId=t, flatten=True)['sessionId']
        B.call('Fetch.enable', s, patterns=[{'urlPattern': '*lenovoid.wust=*', 'requestStage': 'Request'}])
        B.call('Page.navigate', s, url=LOGIN)
        log('Sign in with your Lenovo ID in the window that just opened...\n')
        deadline = time.time() + 15 * 60
        while True:
            try:
                m = q.get(timeout=max(1, deadline - time.time()))
            except queue.Empty:
                B.call('Target.closeTarget', targetId=t)
                raise Fail('No sign-in within 15 minutes.')
            p = m.get('params', {})
            if m['method'] == 'Fetch.requestPaused' and m.get('sessionId') == s:
                B.call('Fetch.failRequest', s, requestId=p['requestId'], errorReason='Aborted')  # the token is single-use
                B.call('Target.closeTarget', targetId=t)
                log('Signed in. Asking Lenovo for your recovery orders...\n')
                return p['request']['url']
            if m['method'] == 'Target.detachedFromTarget' and p.get('sessionId') == s:
                raise Fail('The sign-in window was closed before signing in.')
    finally:
        B.listeners.remove(q)


# ---- jobs: one at a time, output kept for the UI ---------------------------------------------------

class Fail(Exception):
    pass


JOB = {'name': None, 'running': False, 'ok': None, 'lines': [], 'cur': ''}
PROC = None


def log(text):  # terminal-ish: \r rewinds the current line (progress counters)
    for part in re.split(r'(\r\n|\n|\r)', text):
        if part in ('\n', '\r\n'):
            JOB['lines'].append(JOB['cur'])
            JOB['cur'] = ''
        elif part == '\r':
            JOB['cur'] = ''
        else:
            JOB['cur'] += part
    del JOB['lines'][:-400]


def sh(cmd, input=None):
    global PROC
    PROC = subprocess.Popen(cmd, cwd=HERE, stdin=subprocess.PIPE if input else subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if input:
        PROC.stdin.write(input.encode())
        PROC.stdin.close()
    dec = codecs.getincrementaldecoder('utf-8')('replace')
    for chunk in iter(lambda: PROC.stdout.read1(65536), b''):
        log(dec.decode(chunk))
    if PROC.wait():
        raise Fail()  # the script already printed why


def start(name, *steps):
    if JOB['running']:
        return 'Another step is still running.'
    JOB.update(name=name, running=True, ok=None, lines=[], cur='')

    def go():
        try:
            for step in steps:
                step()
            JOB['ok'] = True
        except Fail as e:
            JOB['ok'] = False
            e.args and log('\n' + e.args[0])
        except Exception as e:
            JOB['ok'] = False
            log('\nError: %r' % e)
        JOB['cur'] and log('\n')
        JOB['running'] = False
    threading.Thread(target=go, daemon=True).start()


def order_files():
    try:
        return list(ET.parse(ddrs.ORDER_XML).getroot().find('downloadResponse').iter('file'))
    except (OSError, ET.ParseError, AttributeError):
        return []


def as_root(cmd):
    return cmd if MAC else ['pkexec'] + cmd


def act_login(_):
    return start('login', lambda: sh(PY + [DDRS, 'login', '-'], input=capture_wust()))


def act_order(body):
    oid = str(body.get('id', ''))
    if not re.fullmatch(r'[0-9A-Za-z-]{1,64}', oid):
        return 'bad order id'

    def fetch_rmf():  # small layout file, checked before committing to the big download
        rmf = [f.get('name') for f in order_files() if f.get('name', '').upper().endswith('.RMF')]
        rmf and sh(PY + [DDRS, 'download'] + rmf)
    return start('order', lambda: sh(PY + [DDRS, 'order', oid]), fetch_rmf)


def act_download(_):
    return start('download', lambda: sh(PY + [DDRS, 'download']), lambda: sh(PY + [BUILD, 'assets']))


def act_build(body, step='build'):
    disk = str(body.get('disk', ''))
    if not any(d['id'] == disk and not d['problem'] for d in build_usb.list_disks()):
        return 'That disk is no longer available.'
    return start('build', lambda: sh(PY + [BUILD, 'assets']), lambda: sh(as_root(PY + [BUILD, step, disk])))


ACTIONS = {'/api/login': act_login, '/api/order': act_order, '/api/download': act_download,
           '/api/build': act_build, '/api/rawfix': lambda b: act_build(b, 'rawfix')}


def status():
    try:
        sess = ddrs.load()
    except (OSError, ValueError):
        sess = {}
    files = [f for f in order_files() if f.get('source')]
    total = done = 0
    for f in files:
        size = float(f.get('size') or 0) * UNITS.get((f.get('stype') or 'B').upper(), 1)
        p = os.path.join(ddrs.FILES, os.path.basename(f.get('name')))
        total += size
        done += size if os.path.exists(p) else min(size, os.path.getsize(p + '.tmp')) if os.path.exists(p + '.tmp') else 0
    try:
        dr = ET.parse(ddrs.ORDER_XML).getroot().find('downloadResponse')
        order = dict(dr.attrib, files=len(files), total=total, done=done)
    except (OSError, ET.ParseError, AttributeError):
        order = None
    have_rmf = bool(glob.glob(os.path.join(ddrs.FILES, '*.RMF')))
    problem = build_usb.layout_problem() if have_rmf else None
    missing = None if problem or not have_rmf else sum(not os.path.exists(build_usb.local(f)) for f in build_usb.manifest()[1])
    tools = [] if build_usb.SEVENZIP else ['7-Zip']
    if not MAC:
        sbin = os.environ.get('PATH', '') + ':/usr/sbin:/sbin'
        tools += [t for t in ('lsblk', 'sfdisk', 'wipefs', 'mkfs.vfat', 'pkexec') if not shutil.which(t, path=sbin)]
    os.makedirs(ddrs.FILES, exist_ok=True)
    return {'mac': MAC, 'signedIn': bool(sess.get('token')), 'orders': sess.get('orders', []), 'order': order,
            'orderId': sess.get('orderId'), 'layoutProblem': problem, 'ready': missing == 0, 'missingTools': tools,
            'free': shutil.disk_usage(ddrs.FILES).free, 'job': dict(JOB, lines=JOB['lines'][-200:])}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body, ctype='application/json'):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        self.wfile.write(data)

    def allowed(self):  # the token lives in the app window's URL fragment; other sites can't send it
        return self.headers.get('Host') == HOST and secrets.compare_digest(self.headers.get('X-Token', ''), TOKEN)

    def do_GET(self):
        if self.path == '/' and self.headers.get('Host') == HOST:
            return self.send(200, open(INDEX, 'rb').read(), 'text/html; charset=utf-8')
        if not self.allowed():
            return self.send(403, {'error': 'forbidden'})
        if self.path == '/api/status':
            return self.send(200, status())
        if self.path == '/api/disks':
            try:
                return self.send(200, build_usb.list_disks())
            except (OSError, subprocess.CalledProcessError, ValueError):
                return self.send(200, [])
        self.send(404, {'error': 'not found'})

    def do_POST(self):
        if not self.allowed() or self.path not in ACTIONS:
            return self.send(403, {'error': 'forbidden'})
        body = json.loads(self.rfile.read(int(self.headers.get('Content-Length') or 0)) or b'{}')
        err = ACTIONS[self.path](body)
        self.send(409 if err else 200, {'error': err})


def main():
    global B, HOST
    exe = find_browser()
    if not exe:
        sys.exit('This app needs Google Chrome, Chromium, Microsoft Edge or Brave (also used for the Lenovo ID sign-in).')
    srv = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    HOST = '127.0.0.1:%d' % srv.server_port
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    B = Browser(exe, 'http://%s/#%s' % (HOST, TOKEN))
    print('The app is open in a %s window. Close that window to quit.' % os.path.basename(exe))
    try:
        B.closed.wait()
    except KeyboardInterrupt:
        pass
    if JOB['running'] and PROC:
        try:
            PROC.terminate()
        except OSError:
            pass
    B.quit()


if __name__ == '__main__':
    main()
