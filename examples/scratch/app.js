// Scratch Pad — local-first notes. No server round-trip: localStorage.
const KEY = 'scratch.notes';
const $ = id => document.getElementById(id);
let notes = [];
try { notes = JSON.parse(localStorage.getItem(KEY) || '[]'); } catch {}

function save() { localStorage.setItem(KEY, JSON.stringify(notes)); }

function render() {
  $('empty').style.display = notes.length ? 'none' : 'block';
  $('notes').replaceChildren(...notes.map((n, i) => {
    const li = document.createElement('li');
    const span = document.createElement('span');
    span.textContent = n.text;
    const when = document.createElement('time');
    when.textContent = new Date(n.ts).toLocaleString();
    const del = document.createElement('button');
    del.textContent = '×';
    del.title = 'delete';
    del.onclick = () => { notes.splice(i, 1); save(); render(); };
    li.append(span, when, del);
    return li;
  }));
}

$('f').onsubmit = e => {
  e.preventDefault();
  const t = $('t').value.trim();
  if (!t) return;
  notes.unshift({ text: t, ts: Date.now() });
  $('t').value = '';
  save();
  render();
};

function net() {
  $('net').textContent = navigator.onLine ? 'online' : 'offline';
  $('net').className = navigator.onLine ? 'on' : 'off';
}
addEventListener('online', net);
addEventListener('offline', net);
net();
render();
