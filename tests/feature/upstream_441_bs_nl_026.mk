define FOO
$(foreach
  a 	
 , b	
 c  ,$(info
  $a    )
    )
endef
all:;@:$(FOO)
