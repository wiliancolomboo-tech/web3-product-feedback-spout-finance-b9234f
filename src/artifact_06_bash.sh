# Terminal 1: Start local validator with extended compute limits
solana-test-validator \
  --reset \
  --limit-ledger-size 50000000 \
  --bpf-program FsJ3A3u2vn5cTVofMrvy65GZreG57vEupuzSRWB5Fra8 tests/fixtures/pyth_oracle.so