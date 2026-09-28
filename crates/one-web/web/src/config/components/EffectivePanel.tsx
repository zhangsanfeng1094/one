import { useEffect, useState } from 'react';
import { fetchEffective } from '../api';
import type { EffectiveReport, ModuleId } from '../types';

export function EffectivePanel({ module }: { module: ModuleId }) {
  const [report, setReport] = useState<EffectiveReport | null>(null);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    let active = true;
    setReport(null); setError('');
    fetchEffective(module).then(result => { if (active) setReport(result); })
      .catch(e => { if (active) setError(String(e)); });
    return () => { active = false; };
  }, [module, revision]);
  return <section className="cfg-panel">
    <header className="cfg-panel-head"><h3>最终全局配置</h3><button className="cfg-btn" onClick={() => setRevision(r => r + 1)}>重新读取</button></header>
    <p className="cfg-source-chain">内置默认 → 全局自定义 → 最终生效</p>
    {error && <p role="alert">{error}</p>}
    {!report && !error && <p>正在读取…</p>}
    {report && <>
      <p className="cfg-note">{report.note}</p>
      <section aria-label="可读预览"><h4>可读预览</h4>
        {report.entries.length ? report.entries.map(entry => <p key={entry.name}><b>{entry.name}</b>：{entry.value}</p>) : <p>暂无自定义条目。</p>}
      </section>
      <details><summary>高级 / 原始配置与来源</summary><pre className="cfg-enhancer-prompt">{JSON.stringify(report, null, 2)}</pre></details>
    </>}
  </section>;
}
