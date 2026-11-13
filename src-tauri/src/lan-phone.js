const $ = id => document.getElementById(id);
const state = { ticket:sessionStorage.getItem('desklet-ticket'), busy:false, polling:false, signature:'', draft:[] };
const imageLoads = new WeakMap();
const images = new IntersectionObserver(entries => {
 for (const entry of entries) {
  if (!entry.isIntersecting) continue;
  images.unobserve(entry.target);
  imageLoads.get(entry.target)?.().catch(() => {});
 }
}, { root:$('inbox'), rootMargin:'220px' });
const code = location.hash.slice(1);
if (code) history.replaceState(null, '', location.pathname);

function status(text) { $('status').textContent = text; }

async function api(path, opts = {}) {
 const headers = new Headers(opts.headers);
 if (state.ticket) headers.set('Authorization', 'Bearer ' + state.ticket);
 const response = await fetch('/api/' + path, { ...opts, headers });
 if (!response.ok) {
  const error = new Error(await response.text() || '连接失败');
  error.code = response.status;
  throw error;
 }
 return response;
}

async function poll() {
 if (state.polling) return;
 state.polling = true;
 try {
  const view = await (await api('status')).json();
  $('transfer').hidden = !view.approved;
  $('notice').hidden = view.approved;
  if (!view.approved) status('等待电脑确认连接…');
  else if (!state.busy && ['正在连接…','等待电脑确认连接…','连接暂时中断'].includes($('status').textContent))
   status('已连接');
  const signature = JSON.stringify(view.items);
  if (state.signature !== signature) {
   state.signature = signature;
   render(view.items);
  }
 } catch (error) {
  $('transfer').hidden = true;
  state.signature = '';
  render([]);
  if (error.code === 401 || error.code === 410) {
   state.ticket = null;
   sessionStorage.removeItem('desklet-ticket');
   $('text').value = '';
   state.draft.length = 0;
   showDraft();
   status('连接已断开，请在电脑上重新开启并扫码。');
  } else if (state.ticket) status('连接暂时中断');
 } finally {
  state.polling = false;
 }
}

function button(box, label, task) {
 const control = document.createElement('button');
 control.type = 'button';
 control.textContent = label;
 control.onclick = async () => {
  const menu = control.closest('.menu');
  if (menu) menu.open = false;
  control.disabled = true;
  try { await task(); }
  catch (error) { status(error.message); }
  finally { control.disabled = false; }
 };
 box.append(control);
 return control;
}

function render(items) {
 const box = $('inbox');
 const atBottom = box.scrollHeight - box.clientHeight - box.scrollTop < 40;
 const ids = new Set(items.map(item => item.id));
 for (const row of [...box.children]) {
  if (ids.has(row.dataset.id)) continue;
  images.unobserve(row);
  const image = row.querySelector('img');
  if (image) URL.revokeObjectURL(image.src);
  row.remove();
 }
 if (!items.length) {
  const empty = document.createElement('div');
  empty.className = 'empty';
  empty.textContent = '发条消息，或添加图片、文件';
  box.append(empty);
  return;
 }
 const existing = new Set([...box.children].map(row => row.dataset.id));
 for (const item of items) {
  if (existing.has(item.id)) continue;
  const row = document.createElement('div');
  row.className = item.incoming ? 'row mine' : 'row';
  row.dataset.id = item.id;
  row.setAttribute('aria-label', item.incoming ? '我发送的消息' : '来自电脑');
  const bubble = document.createElement('div');
  bubble.className = 'bubble';
  const controls = document.createElement('div');
  controls.className = 'controls';
  const read = async () => (await api('view/' + item.id)).text();
  if (item.kind === 'text' || item.kind === 'link') {
   const preview = document.createElement(item.kind === 'link' ? 'a' : 'pre');
   preview.textContent = item.preview;
   if (item.kind === 'link') {
    preview.className = 'message-link';
    preview.target = '_blank';
    preview.rel = 'noopener noreferrer';
    const setLink = value => {
     const url = new URL(value);
     if (!/^https?:$/.test(url.protocol) || url.username || url.password) throw new Error('无法打开此链接。');
     preview.href = url.href;
    };
    try {
     if (new TextEncoder().encode(item.preview).length === item.size && !item.preview.endsWith('…')) setLink(item.preview);
    } catch {}
    if (!preview.href) {
     preview.href = '#';
     preview.onclick = async event => {
      event.preventDefault();
      try { setLink(await read());preview.onclick = null;preview.click(); }
      catch (error) { status(error.message); }
     };
    }
   }
   bubble.append(preview);
   if (new TextEncoder().encode(item.preview).length !== item.size || item.preview.endsWith('…'))
    button(controls, '查看全文', async () => { preview.textContent = await read(); });
   button(controls, '复制文字', async () => {
    const value = await read();
    if (navigator.clipboard) await navigator.clipboard.writeText(value);
    else {
     const field = document.createElement('textarea');
     field.value = value;
     field.readOnly = true;
     bubble.append(field);
     field.select();
     field.setSelectionRange(0, value.length);
     const copied = document.execCommand('copy');
     field.remove();
     if (!copied) {
      preview.textContent = value;
      throw new Error('请长按消息文字复制。');
     }
    }
    status('文字已复制。');
   });
  } else {
   const name = document.createElement('strong');
   name.textContent = item.name;
   const size = document.createElement('small');
   size.textContent = ' · ' + (item.size < 1024 ? item.size + ' B' : Math.ceil(item.size / 1024) + ' KB');
   if (item.kind === 'image') {
    const image = document.createElement('img');
    image.alt = item.name;
    image.title = item.name;
    const loading = document.createElement('span');
    loading.textContent = '图片 · ' + item.name;
    bubble.append(loading);
    const show = async () => {
     if (image.isConnected) return;
     let href;
     try {
      const blob = await (await api('view/' + item.id)).blob();
      if (!row.isConnected) return;
      href = URL.createObjectURL(blob);
      image.src = href;
      await image.decode();
      if (!row.isConnected) { URL.revokeObjectURL(href);return; }
      const follow = box.scrollHeight - box.clientHeight - box.scrollTop < 40;
      loading.replaceWith(image);
      if (follow) box.scrollTop = box.scrollHeight;
     } catch {
      if (href) URL.revokeObjectURL(href);
      loading.textContent = '无法显示 · ' + item.name;
     }
    };
    imageLoads.set(row, show);
    images.observe(row);
   } else {
    const card = document.createElement('div');
    card.className = 'file-card';
    const icon = document.createElement('span');
    icon.className = 'file-icon';
    icon.textContent = '▤';
    icon.setAttribute('aria-hidden', 'true');
    const info = document.createElement('div');
    info.append(name, size);
    card.append(icon, info);
    bubble.append(card);
    if (item.preview !== undefined) {
     const preview = document.createElement('pre');
     preview.className = 'excerpt';
     preview.textContent = item.preview;
     bubble.append(preview);
     button(controls, '查看全文', async () => {
      preview.textContent = await read();
      preview.classList.remove('excerpt');
     });
    }
   }
   if (!item.incoming) button(controls, '下载', async () => {
    const blob = await (await api('download/' + item.id)).blob();
    const href = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = href;
    link.download = item.name;
    link.click();
    setTimeout(() => URL.revokeObjectURL(href), 30000);
   });
  }
  if (controls.childElementCount) {
   bubble.classList.add('has-menu');
   const menu = document.createElement('details');
   menu.className = 'menu';
   const more = document.createElement('summary');
   more.textContent = '⋯';
   more.setAttribute('aria-label', '消息操作');
   menu.append(more, controls);
   bubble.append(menu);
  }
  row.append(bubble);
  box.append(row);
 }
 if (atBottom) box.scrollTop = box.scrollHeight;
}

function showDraft() {
 const box = $('draft');
 box.replaceChildren();
 state.draft.forEach((file, index) => {
  const chip = button(box, file.name + ' ×', () => {
   if (!state.busy) { state.draft.splice(index, 1);showDraft(); }
  });
  chip.setAttribute('aria-label', '移除附件 ' + file.name);
 });
}

function attach(files) {
 if (state.busy) return;
 if (files.some(f => !f.size || f.size > 32 * 1024 * 1024)) {
  status('文件为空或超过 32 MB。');
  return;
 }
 state.draft.push(...files);
 showDraft();
}

$('attach').onclick = () => $('file').click();
$('file').onchange = () => { attach([...$('file').files]);$('file').value = ''; };
$('text').addEventListener('paste', event => {
 const files = [...(event.clipboardData?.files || [])];
 if (!files.length) return;
 event.preventDefault();
 attach(files);
});

$('sendText').onclick = async () => {
 if (state.busy || !state.ticket) return;
 const value = $('text').value;
 const text = value.trim();
 if (!text && !state.draft.length) { status('请输入消息或添加附件。');return; }
 state.busy = true;
 $('attach').disabled = $('sendText').disabled = true;
 status('正在发送…');
 try {
  if (text) {
   let url;
   try { url = new URL(text); } catch {}
   const link = url && /^https?:$/.test(url.protocol) && !/\s/.test(text);
   await api('text', { method:'POST', headers:{ 'Content-Type':'application/json' },
    body:JSON.stringify({ name:link ? url.hostname : '文字', kind:link ? 'link' : 'text', text:link ? url.href : text }) });
   if ($('text').value === value) $('text').value = '';
  }
  while (state.draft.length) {
   const file = state.draft[0];
   const name = btoa(Array.from(new TextEncoder().encode(file.name), c => String.fromCharCode(c)).join(''));
   await api('file', { method:'POST', headers:{ 'x-desklet-name':name, 'Content-Type':'application/octet-stream' }, body:file });
   state.draft.shift();
   showDraft();
  }
  status('已发送');
 } catch (error) { status(error.message); }
 finally {
  state.busy = false;
  $('attach').disabled = $('sendText').disabled = false;
  await poll();
 }
};

(async () => {
 try {
  if (code && !state.ticket) {
   const view = await (await api('pair', { method:'POST', headers:{ 'x-desklet-code':code } })).json();
   state.ticket = view.id;
   sessionStorage.setItem('desklet-ticket', view.id);
  }
  if (!state.ticket) { status('请从电脑扫码，或使用包含配对码的完整地址。');return; }
  await poll();
  setInterval(() => { if (state.ticket) poll(); }, 1800);
 } catch (error) { status(error.message); }
})();
