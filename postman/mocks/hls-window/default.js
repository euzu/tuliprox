const http = require('http');

// Reuses the existing hls-window fixture. Optional controls advance origin
// metadata while a loopback streaming fixture gates actual TS responses.
http.createServer(async (req, res) => {
  if (req.method === 'POST' && req.url === '/configure') {
    const config = req.body;
    if (!config || !/^http:\/\/127\.0\.0\.1:\d+$/.test(config.segmentOrigin) ||
        !Number.isSafeInteger(config.firstSequence) || config.firstSequence < 0) {
      res.writeHead(400);
      return res.end('invalid loopback fixture configuration');
    }
    await pm.state.set('hls-window.config', config);
    res.writeHead(204);
    return res.end();
  }
  if (req.method === 'GET' && req.url === '/index.m3u8') {
    const config = await pm.state.get('hls-window.config');
    const first = config ? config.firstSequence : 0;
    const segmentOrigin = config ? config.segmentOrigin + '/' : '';
    res.writeHead(200, { 'content-type': 'application/vnd.apple.mpegurl' });
    return res.end('#EXTM3U\n#EXT-X-TARGETDURATION:4\n' +
      `#EXT-X-MEDIA-SEQUENCE:${first}\n` +
      Array.from({ length: 6 }, (_, i) => `#EXTINF:4,\n${segmentOrigin}${first + i}.ts\n`).join(''));
  }
  if (req.method === 'GET' && /^\/[0-5]\.ts$/.test(req.url)) {
    res.writeHead(200, { 'content-type': 'video/mp2t', 'content-length': 12 });
    return setTimeout(() => {
      res.write('segment-');
      setTimeout(() => res.end('body'), 350);
    }, 30);
  }
  res.writeHead(501);
  res.end('endpoint outside the HLS window fixture contract');
}).listen(process.env.PORT);
