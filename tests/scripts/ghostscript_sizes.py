#!/usr/bin/env python3
"""Record Ghostscript reference sizes for every frozen corpus entry (test only)."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--manifest',type=Path,required=True)
    p.add_argument('--input-dir',type=Path,required=True)
    p.add_argument('--output-dir',type=Path,required=True)
    p.add_argument('--preset',choices=['lossless','prepress'],required=True)
    p.add_argument('--timeout',type=int,default=90)
    p.add_argument('--workers',type=int,default=2)
    a=p.parse_args()
    a.output_dir.mkdir(parents=True,exist_ok=False)
    gs=shutil.which('gs');qpdf=shutil.which('qpdf')
    if not gs or not qpdf:raise SystemExit('gs and qpdf are required')
    data=a.manifest.read_bytes();manifest=json.loads(data)
    (a.output_dir/'manifest.json').write_bytes(data)
    settings=['-dPDFSETTINGS=/prepress','-dCompatibilityLevel=1.7','-dDetectDuplicateImages=true','-dCompressFonts=true']
    if a.preset=='lossless':
        settings += ['-dDownsampleColorImages=false','-dDownsampleGrayImages=false','-dDownsampleMonoImages=false','-dAutoFilterColorImages=false','-dAutoFilterGrayImages=false','-dColorImageFilter=/FlateEncode','-dGrayImageFilter=/FlateEncode']
    metadata={'manifest_sha256':hashlib.sha256(data).hexdigest(),'preset':a.preset,'settings':settings,
              'gs_version':subprocess.check_output([gs,'--version'],text=True).strip(),
              'qpdf_version':subprocess.check_output([qpdf,'--version'],text=True).splitlines()[0],
              'started_unix':time.time()}
    (a.output_dir/'run.json').write_text(json.dumps(metadata,indent=2))
    def run(entry):
        source=a.input_dir/entry['local_path'];output=a.output_dir/source.name
        result={'file':entry['local_path'],'evidence_class':entry['evidence_class']}
        if not source.exists():return dict(result,status='input_missing')
        digest=hashlib.sha256(source.read_bytes()).hexdigest()
        if digest!=entry['sha256']:return dict(result,status='input_hash_mismatch')
        result.update(input_sha256=digest,input_bytes=source.stat().st_size)
        command=[gs,'-q','-dSAFER','-dBATCH','-dNOPAUSE','-dPDFSTOPONERROR','-dPDFSTOPONWARNING','-sDEVICE=pdfwrite',*settings,'-sOutputFile='+str(output.resolve()),str(source.resolve())]
        result['command']=command
        start=time.monotonic()
        try:
            process=subprocess.run(command,capture_output=True,timeout=a.timeout)
            (a.output_dir/(source.stem+'.log')).write_bytes(process.stdout+process.stderr)
            result['returncode']=process.returncode
            if process.returncode!=0:return dict(result,status='gs_failed')
            check=subprocess.run([qpdf,'--check',str(output)],capture_output=True,timeout=a.timeout)
            (a.output_dir/(source.stem+'.qpdf.log')).write_bytes(check.stdout+check.stderr)
            return dict(result,status='passed' if check.returncode==0 else 'qpdf_failed',qpdf_returncode=check.returncode,
                        output_bytes=output.stat().st_size,output_sha256=hashlib.sha256(output.read_bytes()).hexdigest(),
                        elapsed_seconds=time.monotonic()-start)
        except subprocess.TimeoutExpired:return dict(result,status='timeout')
        except Exception as e:return dict(result,status='error',error=str(e))
    with (a.output_dir/'results.jsonl').open('w') as stream:
        with ThreadPoolExecutor(max_workers=a.workers) as pool:
            rows=[]
            for row in pool.map(run,manifest['entries']):
                rows.append(row);stream.write(json.dumps(row)+'\n');stream.flush()
    from collections import Counter
    summary={'outcomes':dict(Counter(r['status'] for r in rows)),'elapsed_seconds':time.time()-metadata['started_unix']}
    (a.output_dir/'summary.json').write_text(json.dumps(summary,indent=2));print(json.dumps(summary))

if __name__=='__main__':main()
