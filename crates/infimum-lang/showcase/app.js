(() => {
  'use strict';

  const dec = new TextDecoder();
  const enc = new TextEncoder();

  const els = {
    wasmBadge: document.getElementById('wasm-size-badge'),
    engineStatus: document.getElementById('engine-status'),
    rootHashCode: document.getElementById('root-hash-code'),
    form: document.getElementById('exec-form'),
    tokenInput: document.getElementById('token-input'),
    amountInput: document.getElementById('amount-input'),
    presetValidBtn: document.getElementById('preset-valid-btn'),
    presetInvalidBtn: document.getElementById('preset-invalid-btn'),
    execBanner: document.getElementById('exec-banner'),
    slider: document.getElementById('timeline-slider'),
    frameIndicator: document.getElementById('frame-indicator'),
    resetBtn: document.getElementById('reset-timeline-btn'),
    callsList: document.getElementById('calls-list'),
    effectsList: document.getElementById('effects-list'),
    passportBox: document.getElementById('passport-box'),
    dedupList: document.getElementById('dedup-list'),
    tabStubBtn: document.getElementById('tab-stub-btn'),
    tabLeakBtn: document.getElementById('tab-leak-btn'),
    xraySource: document.getElementById('xray-source'),
    xrayDiag: document.getElementById('xray-diag'),
  };

  let wasmExports = null;
  let bundleData = null;

  function clearChildren(node) {
    while (node.firstChild) {
      node.removeChild(node.firstChild);
    }
  }

  function makeLi(text, className) {
    const li = document.createElement('li');
    if (className) {
      li.className = className;
    }
    li.textContent = text;
    return li;
  }

  function syncSliderCssProps() {
    const minVal = Number(els.slider.min || 0);
    const maxVal = Math.max(1, Number(els.slider.max || 1));
    const curVal = Number(els.slider.value || 0);
    els.slider.style.setProperty('--attr-min', String(minVal));
    els.slider.style.setProperty('--attr-max', String(maxVal));
    els.slider.style.setProperty('--control-value', String(curVal));
  }

  function readWasmJson() {
    const ptr = wasmExports.infimum_out_ptr();
    const len = wasmExports.infimum_out_len();
    const slice = new Uint8Array(wasmExports.memory.buffer, ptr, len);
    return JSON.parse(dec.decode(slice));
  }

  function showBanner(text, isError) {
    els.execBanner.hidden = false;
    els.execBanner.className = isError ? 'banner banner-err' : 'banner banner-ok';
    els.execBanner.textContent = text;
  }

  function renderFrame(frame) {
    const total = frame.total_frames || wasmExports.infimum_frame_count();
    els.slider.disabled = total <= 1;
    els.slider.min = '0';
    els.slider.max = String(Math.max(0, total - 1));
    els.slider.value = String(frame.frame_id);
    const label = `Frame #${frame.frame_id} of ${total} (inputs: ${frame.inputs.join(', ')})`;
    els.slider.setAttribute('aria-valuetext', label);
    els.frameIndicator.textContent = `Frame #${frame.frame_id} / ${total - 1}`;
    syncSliderCssProps();

    clearChildren(els.callsList);
    for (const c of frame.calls) {
      const indent = '  '.repeat(c.depth);
      els.callsList.appendChild(
        makeLi(`${indent}${c.alias} (#${c.node})(${c.args.join(', ')}) => ${c.return}`)
      );
    }

    clearChildren(els.effectsList);
    for (const e of frame.effects) {
      els.effectsList.appendChild(
        makeLi(`[EFFECT #${e.step}] ${e.effect}.${e.op}(${e.args.join(', ')}) via #${e.node}`)
      );
    }
    els.effectsList.appendChild(
      makeLi(`[RETURN] ${frame.result} (raw secret never persisted in memory snapshot)`)
    );
  }

  function executeWasmStep(secretToken, amountStr) {
    if (!wasmExports) return;
    const t0 = performance.now();
    const payload = enc.encode(`${secretToken}\n${amountStr}`);
    const inPtr = wasmExports.infimum_prepare_input(payload.length);
    new Uint8Array(wasmExports.memory.buffer, inPtr, payload.length).set(payload);

    const exitCode = wasmExports.infimum_step();
    const elapsedUs = ((performance.now() - t0) * 1000).toFixed(0);
    const data = readWasmJson();

    if (exitCode === 0) {
      showBanner(
        `Executed #${bundleData ? bundleData.ingested_nodes[3].short_hash : 'checkout'} in ${elapsedUs} us — recorded as Frame #${data.frame_id} (Secret<str> redacted)`,
        false
      );
      renderFrame(data);
    } else {
      showBanner(`Rejected at Wasm boundary (${data.kind}): ${data.message}`, true);
    }
  }

  function renderPassport(passport) {
    clearChildren(els.passportBox);
    els.rootHashCode.textContent = `#${passport.short_hash}`;

    const rows = [
      `Root Node: #${passport.short_hash} (${passport.aliases.join(', ')})`,
      `Verified Subgraph: ${passport.total_reachable_nodes} immutable node(s)`,
    ];
    for (const cap of passport.capabilities) {
      rows.push(`[CAP] ${cap.effect}  via: ${cap.witness_chain.join(' -> ')}`);
    }
    for (const pipe of passport.secret_pipes) {
      rows.push(
        `[PIPE] ${pipe.secret_type} -> ${pipe.operation}("${pipe.destination_domain}")`
      );
    }
    rows.push('[SAFE] Disk / Log / Unpinned Network Leaks: 0 (statically proved)');

    for (const text of rows) {
      const div = document.createElement('div');
      div.className = 'passport-row';
      div.textContent = text;
      els.passportBox.appendChild(div);
    }
  }

  function renderXrayTab(mode) {
    if (!bundleData) return;
    const isStub = mode === 'stub';
    els.tabStubBtn.className = isStub ? 'btn btn-primary' : 'btn btn-secondary';
    els.tabLeakBtn.className = !isStub ? 'btn btn-primary' : 'btn btn-secondary';
    els.tabStubBtn.setAttribute('aria-selected', String(isStub));
    els.tabLeakBtn.setAttribute('aria-selected', String(!isStub));

    const src = isStub ? bundleData.anti_stub_source : bundleData.secret_leak_source;
    const diag = isStub ? bundleData.anti_stub_diagnostic : bundleData.secret_leak_diagnostic;
    els.xraySource.textContent = src.trim();

    clearChildren(els.xrayDiag);
    const title = document.createElement('div');
    title.className = 'diag-title';
    title.textContent = `[${diag.code}] (${diag.phase} phase): ${diag.message}`;
    els.xrayDiag.appendChild(title);

    if (diag.counterexample && Object.keys(diag.counterexample).length > 0) {
      const cexDiv = document.createElement('div');
      cexDiv.className = 'diag-cex';
      const pairs = Object.entries(diag.counterexample).map(([k, v]) => `${k} = ${v}`);
      cexDiv.textContent = `Z3 Counterexample: { ${pairs.join(', ')} }`;
      els.xrayDiag.appendChild(cexDiv);
    }

    const hint = document.createElement('div');
    hint.textContent = `Repair Hint: ${diag.repair_hint}`;
    els.xrayDiag.appendChild(hint);
  }

  async function init() {
    try {
      const [wasmResp, diagResp] = await Promise.all([
        fetch('checkout.wasm'),
        fetch('diagnostics.json'),
      ]);
      const wasmBytes = await wasmResp.arrayBuffer();
      bundleData = await diagResp.json();

      const { instance } = await WebAssembly.instantiate(wasmBytes, {});
      wasmExports = instance.exports;

      const kb = (wasmBytes.byteLength / 1024).toFixed(1);
      els.wasmBadge.textContent = `${kb} KB Wasm Capsule · Zero JS Dependencies`;
      els.engineStatus.textContent = `Wasm Ready (${kb} KB)`;

      wasmExports.infimum_load_passport();
      renderPassport(readWasmJson());

      clearChildren(els.dedupList);
      for (const item of bundleData.ingested_nodes) {
        const tag = item.deduplicated
          ? 'DEDUP: identical canonical AST!'
          : `Z3 proved ${item.obligations_proved} oblig.`;
        els.dedupList.appendChild(makeLi(`#${item.short_hash}  ${item.alias}  [${tag}]`));
      }

      renderXrayTab('stub');

      // Seed 2 initial frames so the Time-Travel slider is live immediately on page load!
      executeWasmStep('sk_live_seed_alpha', '10000');
      executeWasmStep('sk_live_seed_beta', '25000');
    } catch (err) {
      els.engineStatus.textContent = 'Failed to load Wasm';
      showBanner(`Initialization error: ${err.message}`, true);
    }
  }

  els.form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    executeWasmStep(els.tokenInput.value, els.amountInput.value);
  });

  els.presetValidBtn.addEventListener('click', () => {
    els.amountInput.value = '45000';
    executeWasmStep(els.tokenInput.value, '45000');
  });

  els.presetInvalidBtn.addEventListener('click', () => {
    els.amountInput.value = '50';
    executeWasmStep(els.tokenInput.value, '50');
  });

  els.slider.addEventListener('input', () => {
    syncSliderCssProps();
    if (!wasmExports) return;
    const idx = Number(els.slider.value);
    if (wasmExports.infimum_scrub_frame(idx) === 0) {
      renderFrame(readWasmJson());
    }
  });

  els.resetBtn.addEventListener('click', () => {
    if (!wasmExports) return;
    wasmExports.infimum_reset_timeline();
    els.slider.disabled = true;
    els.slider.min = '0';
    els.slider.max = '0';
    els.slider.value = '0';
    els.slider.setAttribute('aria-valuetext', 'No frames recorded');
    els.frameIndicator.textContent = '0 frames';
    syncSliderCssProps();
    clearChildren(els.callsList);
    els.callsList.appendChild(makeLi('Timeline cleared. Click "Execute Node" to record.', 'empty-item'));
    clearChildren(els.effectsList);
    els.effectsList.appendChild(makeLi('Zero side effects executed.', 'empty-item'));
  });

  els.tabStubBtn.addEventListener('click', () => renderXrayTab('stub'));
  els.tabLeakBtn.addEventListener('click', () => renderXrayTab('leak'));

  syncSliderCssProps();
  init();
})();
