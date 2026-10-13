"""Offline failure-custody controls; no native driver or settlement evidence."""
import contextlib
from pathlib import Path
import sys
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.dont_write_bytecode = True
sys.path.insert(0, str(SOURCE))
import cuda_lifecycle as cuda


class MetadataFailure(RuntimeError):
    pass


class RejectingMetadata(RuntimeError):
    def __getattribute__(self, name):
        if name.startswith('_installation_') or name == '__notes__':
            raise MetadataFailure('exception metadata read refused')
        return super().__getattribute__(name)

    def __setattr__(self, name, value):
        if name.startswith('_installation_') or name == '__notes__':
            raise MetadataFailure('exception metadata write refused')
        return super().__setattr__(name, value)


class RejectingNotes(RuntimeError):
    def __init__(self, message):
        super().__init__(message)
        # BaseException.add_note must refuse this invalid notes container.
        self.__notes__ = ()


class PartialInstallTests(unittest.TestCase):
    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch('ctypes.CDLL', side_effect=AssertionError('native loading prohibited')))
        self.native_calls = []
        def forbidden_native(*args):
            self.native_calls.append(args)
            raise AssertionError('native entry prohibited')
        self.bindings = SimpleNamespace(**{name: forbidden_native for name in cuda._ALLOW})
        self.bindings.dll = object()
        self.original_init = self.bindings.cuInit
        package = ModuleType('tinygrad')
        package.__path__ = []
        class DeviceName:
            def __repr__(self):
                return 'CUDA:PTX'
        helpers = SimpleNamespace(
            DEV=DeviceName(), JIT=SimpleNamespace(value=0), PROFILE=SimpleNamespace(value=0),
            DEBUG=SimpleNamespace(value=0), BEAM=SimpleNamespace(value=0),
            VALIDATE_WITH_CPU=SimpleNamespace(value=0), getenv=lambda name, default: default,
        )
        package.helpers = helpers
        device = ModuleType('tinygrad.device')
        device.Buffer = type('Buffer', (), {})
        device.Device = SimpleNamespace(_opened_devices=set())
        self.autogen = ModuleType('tinygrad.runtime.autogen')
        self.stack.enter_context(patch.dict(sys.modules, {'tinygrad': package, 'tinygrad.device': device}))
        # Installation checks these names before imports. The scoped dictionary
        # patch restores any unrelated fixture modules at cleanup.
        sys.modules.pop('tinygrad.runtime.ops_cuda', None)
        sys.modules.pop('tinygrad.runtime.graph.cuda', None)

    def fail_import_at(self, stage, primary):
        def import_module(name):
            if name == stage:
                raise primary
            if name == 'tinygrad.runtime.autogen':
                return self.autogen
            if name == 'tinygrad.runtime.ops_cuda':
                return ModuleType(name)
            raise AssertionError('unexpected dependency import: ' + name)
        return patch.object(cuda.importlib, 'import_module', side_effect=import_module)

    def evidence_in_traceback(self, error, function):
        traceback = error.__traceback__
        while traceback and traceback.tb_frame.f_code.co_name != function:
            traceback = traceback.tb_next
        self.assertIsNotNone(traceback)
        return traceback.tb_frame.f_locals['failure_evidence']

    def capture_install_failure(self, primary, publish, stage='tinygrad.runtime.ops_cuda'):
        with self.fail_import_at(stage, primary):
            try:
                cuda.install(bindings=self.bindings, publish_owner=publish)
            except BaseException as error:
                self.assertIs(error, primary)
                return self.evidence_in_traceback(error, 'install')
        self.fail('partial installation returned success')

    def assert_failed_owner(self, owner, primary):
        self.assertIs(owner._installation_failure, primary)
        with self.assertRaises(cuda.LifecycleError):
            with owner.submission():
                self.fail('failed owner accepted a submission')
        with self.assertRaises(cuda.LifecycleError):
            self.bindings.cuInit(0)
        result = owner.shutdown()
        self.assertEqual(result.status, 'failed')
        self.assertFalse(result.observation_delivered)
        owner.abort_installation(RuntimeError('later failure'))
        self.assertIs(owner.shutdown(), result)
        self.assertIs(owner._installation_failure, primary)
        self.assertFalse(self.native_calls)

    def test_partial_import_mutation_retains_same_failed_owner_and_no_success(self):
        for stage in ('tinygrad.runtime.ops_cuda', 'tinygrad.runtime.graph.cuda'):
            with self.subTest(stage=stage):
                # Each attempted installation needs its own fresh binding object.
                self.bindings = SimpleNamespace(**{name: self.original_init for name in cuda._ALLOW}, dll=object())
                primary = RuntimeError('selected consumer import failed')
                published = []
                evidence = self.capture_install_failure(primary, published.append, stage)
                self.assertEqual(len(published), 1)
                self.assertIs(evidence.owners[0], published[0])
                self.assertIsNot(self.bindings.cuInit, self.original_init)
                self.assertIsNone(self.bindings.dll)
                self.assertIs(self.autogen.cuda, self.bindings)
                self.assert_failed_owner(published[0], primary)

    def test_publication_failure_fences_before_first_consumer_mutation(self):
        primary = RuntimeError('publication refused')
        published = []
        def publish(owner):
            published.append(owner)
            raise primary
        with patch.object(cuda, '_install_consumers', side_effect=AssertionError('mutation before publication')) as mutate:
            evidence = self.capture_install_failure(primary, publish)
        mutate.assert_not_called()
        self.assertIs(self.bindings.cuInit, self.original_init)
        self.assertIs(evidence.owners[0], published[0])
        self.assertIs(published[0]._installation_failure, primary)
        self.assertEqual(published[0].shutdown().status, 'failed')
        self.assertFalse(self.native_calls)

    def test_invalid_publication_callback_refuses_before_owner_or_consumer_creation(self):
        with patch.object(cuda, 'Lifecycle', side_effect=AssertionError('owner created')) as create, \
                patch.object(cuda, '_install_consumers', side_effect=AssertionError('consumer mutated')) as mutate:
            with self.assertRaisesRegex(cuda.LifecycleError, 'publication callback'):
                cuda.install(bindings=self.bindings, publish_owner=object())
        create.assert_not_called()
        mutate.assert_not_called()
        self.assertIs(self.bindings.cuInit, self.original_init)
        self.assertFalse(self.native_calls)

    def test_hostile_metadata_keeps_primary_and_actual_secondary_in_local_holder(self):
        for exception_type in (RejectingMetadata, RejectingNotes):
            with self.subTest(exception_type=exception_type):
                primary = exception_type('original installation error')
                secondary = ValueError('first owner fence failed')
                first = cuda.Lifecycle(self.bindings, 0, None)
                second = cuda.Lifecycle(self.bindings, 0, None)
                evidence = cuda._InstallationFailureEvidence()
                evidence.owners.extend((first, second, first))
                attempted = []
                def fail_first(error):
                    attempted.append(('first', error))
                    raise secondary
                original_abort = second.abort_installation
                def second_fence(error):
                    attempted.append(('second', error))
                    original_abort(error)
                with patch.object(first, 'abort_installation', side_effect=fail_first), \
                        patch.object(second, 'abort_installation', side_effect=second_fence):
                    returned = cuda._fence_installation_owners(primary, evidence)
                self.assertIs(returned, evidence)
                self.assertEqual([name for name, _ in attempted], ['first', 'second'])
                self.assertTrue(all(error is primary for _, error in attempted))
                self.assertIs(evidence.owners[0], first)
                self.assertIs(evidence.fence_failures[0], secondary)
                self.assertTrue(evidence.evidence_failures)
                self.assertIs(second._installation_failure, primary)
                self.assertFalse(self.native_calls)

    def test_install_traceback_retains_holder_when_metadata_attachment_is_refused(self):
        primary = RejectingMetadata('original consumer import failure')
        published = []
        evidence = self.capture_install_failure(primary, published.append)
        self.assertIs(evidence.owners[0], published[0])
        self.assertTrue(evidence.evidence_failures)
        self.assert_failed_owner(published[0], primary)

    def test_install_helper_error_cannot_replace_primary_or_discard_local_owner(self):
        primary = RuntimeError('original consumer import failure')
        secondary = ValueError('helper failed after fencing')
        actual_fence = cuda._fence_installation_owners
        def fence(error, evidence):
            actual_fence(error, evidence)
            raise secondary
        published = []
        with patch.object(cuda, '_fence_installation_owners', side_effect=fence):
            evidence = self.capture_install_failure(primary, published.append)
        self.assertIs(evidence.owners[0], published[0])
        self.assertIs(evidence.evidence_failures[-1], secondary)
        self.assert_failed_owner(published[0], primary)


if __name__ == '__main__':
    unittest.main()
