# Copyright 2018 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).

go_swagger_library(
    name='petstore-go',
    sources=['./swagger.json'],
)

# TODO(apratti): remove this after gen.go-swagger in pants.ini is fixed
target(
  name = 'swagger-deps',
  dependencies = [
  ]
)