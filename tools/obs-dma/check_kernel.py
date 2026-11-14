import pathlib,json,sys,subprocess
root=pathlib.Path(__file__).resolve().parent;k=pathlib.Path(sys.argv[1])
def config(p):return dict(x.split('=',1) for x in p.read_text().splitlines() if x.startswith('CONFIG_') and '=' in x)
expected=config(root/'stock.config');actual=config(k/'.config');delta={x:(expected.get(x),actual.get(x)) for x in expected.keys()|actual.keys() if expected.get(x)!=actual.get(x)}
assert not delta,json.dumps(delta,indent=2)
assert (k/'include/config/kernel.release').read_text().strip()=='6.12.77'
symbols={line.split()[1]:line.split()[0] for line in (k/'Module.symvers').read_text().splitlines()}
stock=json.loads((root/'stock-symbols.json').read_text());bad={n:(v,symbols.get(n)) for n,v in stock.items() if symbols.get(n)!=v};assert not bad,bad
print('exact running config, kernel release, and',len(stock),'stock-module symbol CRCs PASS')
