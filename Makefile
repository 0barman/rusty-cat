.PHONY: test-e2e-local

test-e2e-local:
	cargo test --test download_test
	cargo test --test upload_test
	cargo test --test chunk_retry_test
	cargo test --test callback_panic_test
	cargo test --test download_protocol_validation_test
	cargo test --test meow_client_lifecycle_test
	cargo test --test meow_client_global_listener_test
	cargo test --test meow_client_enqueue_snapshot_test
	cargo test --test meow_client_duplicate_and_state_test
	cargo test --test meow_client_listener_misc_test
	cargo test --test meow_config_test
	cargo test --test log_facade_test
	cargo test --test error_and_status_branch_test
	cargo test --test pounce_builder_branch_test
	cargo test --test pounce_builder_api_surface_test
	cargo test --test log_and_record_api_test
	cargo test --test transfer_protocol_edge_case_test
	cargo test --test meow_client_additional_api_test
	cargo test --test transfer_task_bridge_test
	cargo test --test transfer_protocol_more_edge_test
	cargo test --test executor_queue_control_test
	cargo test --test upload_sign_and_duplicate_test
	cargo test --test meow_config_concurrency_e2e_test
