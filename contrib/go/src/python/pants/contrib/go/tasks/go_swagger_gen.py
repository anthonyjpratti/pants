# Copyright 2018 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).

import os
import re
import subprocess

from twitter.common.collections import OrderedSet

from pants.backend.codegen.swagger.subsystems.swagger import Swagger
from pants.base.build_environment import get_buildroot
from pants.base.exceptions import TaskError
from pants.base.workunit import WorkUnitLabel
from pants.option.custom_types import target_option
from pants.task.simple_codegen_task import SimpleCodegenTask
from pants.util.dirutil import safe_mkdir
from pants.util.memo import memoized_property
from pants.util.strutil import safe_shlex_join

from pants.contrib.go.subsystems.swagger_gen_go import SwaggerGenGo
from pants.contrib.go.targets.go_swagger_library import GoSwaggerGenLibrary, GoSwaggerLibrary


class GoSwaggerGen(SimpleCodegenTask):

  sources_globs = ('**/*',)

  @classmethod
  def register_options(cls, register):
    super().register_options(register)
    register('--import-target', type=target_option, fingerprint=True,
             help='Target that will be added as a dependency of swagger-generated Go code.')

  @classmethod
  def subsystem_dependencies(cls):
    return super().subsystem_dependencies() + (Swagger.scoped(cls), SwaggerGenGo,)

  @memoized_property
  def _swagger(self):
    return Swagger.scoped_instance(self).select(context=self.context)

  def synthetic_target_extra_dependencies(self, target, target_workdir):
    import_target = self.get_options().import_target
    if import_target is None:
      raise TaskError('Option import_target in scope {} must be set.'.format(
        self.options_scope))
    return self.context.resolve(import_target)

  def synthetic_target_type(self, target):
    return GoSwaggerGenLibrary

  def is_gentarget(self, target):
    return isinstance(target, GoSwaggerLibrary)

  @classmethod
  def product_types(cls):
    return ['go']

  def execute_codegen(self, target, target_workdir):
    target_cmd = [
      self._swagger,
      'generate',
      'server',
    ]

    swagger_gen_go = SwaggerGenGo.global_instance().select(self.context)
    env = os.environ.copy()
    env['PATH'] = ':'.join([os.path.dirname(swagger_gen_go), env['PATH']])
    # NB: swagger errors out unless --target is within $GOPATH/src!
    env['GOPATH'] = target_workdir

    # NB: make the output directory usable as a go import path!
    # Is this double call to synthetic_target_dir needed?
    outdir = self.synthetic_target_dir(target, target_workdir)
    safe_mkdir(outdir)
    target_cmd.append('--target={}'.format(outdir))

    for source in target.sources_relative_to_buildroot():
      file_cmd = target_cmd + [f'--spec={source}']
      with self.context.new_workunit(name=f'compile {source} with swagger!',
                                     labels=[WorkUnitLabel.TOOL, WorkUnitLabel.COMPILER],
                                     cmd=safe_shlex_join(file_cmd)) as workunit:
        self.context.log.info(safe_shlex_join(file_cmd))
        result = subprocess.call(file_cmd,
                                 env=env,
                                 stdout=workunit.output('stdout'),
                                 stderr=workunit.output('stderr'))
        if result != 0:
          raise TaskError('{} ... exited non-zero ({})'.format(self._swagger, result),
                          exit_code=result)

  @property
  def _copy_target_attributes(self):
    return [a for a in super()._copy_target_attributes if a != 'provides']

  def synthetic_target_dir(self, target, target_workdir):
    all_sources = list(target.sources_relative_to_buildroot())
    source = all_sources[0]
    namespace = self._get_go_namespace(source)
    return os.path.join(target_workdir, 'src', 'go', namespace)

  @classmethod
  def _get_go_namespace(cls, source):
    parts = source.split('/')
    index = parts.index('go')
    return '/'.join(parts[index+1:-1])
