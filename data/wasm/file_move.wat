(module
  (import "host" "file_move" (func $host (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  
  (func (export "run") (param $input_ptr i32) (param $input_len i32) (result i32)
    (local $status i32)
    (local.set $status (call $host (local.get $input_ptr) (local.get $input_len)))
    (if (result i32) (i32.eq (local.get $status) (i32.const 0))
      (then (i32.const 4096))
      (else (i32.const 4096))
    )
  )
  
  (func (export "output_len") (result i32)
    (i32.load (i32.const 4092))
  )
)
