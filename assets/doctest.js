(function () {
  var input = document.querySelector('#search input');
  var box = document.getElementById('results');
  if (!input || !box) return;
  var root = document.body.getAttribute('data-root') || '';
  var lang = document.body.getAttribute('data-lang') || '';
  function esc(s) { return s.replace(/[&<>"]/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]; }); }
  input.addEventListener('input', function () {
    var q = input.value.trim().toLowerCase();
    var items = window.DOCTEST_INDEX || [];
    if (q.length < 2) { box.style.display = 'none'; return; }
    var hits = items.filter(function (it) {
      return (!it.lang || !lang || it.lang === lang) && (it.title.toLowerCase().indexOf(q) >= 0 || it.text.toLowerCase().indexOf(q) >= 0);
    }).slice(0, 12);
    box.innerHTML = hits.length ? hits.map(function (h) {
      var i = h.text.toLowerCase().indexOf(q);
      var snip = i >= 0 ? h.text.substr(Math.max(0, i - 40), 120) : h.text.substr(0, 120);
      return '<a href="' + root + h.url + '">' + esc(h.title) + '<small>' + esc(snip) + '</small></a>';
    }).join('') : '<a>No results</a>';
    box.style.display = 'block';
  });
  document.addEventListener('click', function (e) { if (!e.target.closest('#search')) box.style.display = 'none'; });
})();
