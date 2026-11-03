use plx_core::id::ClientId;
use plx_core::size::{ClientSize, SizePolicy, pane_area};
use plx_core::time::Timestamp;
use proptest::prelude::*;

use crate::strategy::grid_size;

fn clients() -> impl Strategy<Value = Vec<ClientSize>> {
    proptest::collection::vec((grid_size(), 0_u64..20), 0..6).prop_map(|entries| {
        entries
            .into_iter()
            .zip(0_u64..)
            .map(|((size, active), id)| ClientSize {
                client: ClientId::new(id),
                size,
                last_active: Timestamp::from_nanos(active),
            })
            .collect()
    })
}

const POLICIES: [SizePolicy; 3] = [
    SizePolicy::Latest,
    SizePolicy::Smallest,
    SizePolicy::Largest,
];

proptest! {
    #[test]
    fn no_clients_means_no_size(policy in prop::sample::select(POLICIES.to_vec())) {
        prop_assert_eq!(policy.effective(&[]), None);
    }

    #[test]
    fn latest_is_the_most_recently_active_client(clients in clients()) {
        if let Some(size) = SizePolicy::Latest.effective(&clients) {
            let newest = clients.iter().max_by_key(|c| (c.last_active, c.client)).unwrap();
            prop_assert_eq!(size, newest.size);
        }
    }

    #[test]
    fn smallest_and_largest_bound_every_client(clients in clients()) {
        if let (Some(small), Some(large)) =
            (SizePolicy::Smallest.effective(&clients), SizePolicy::Largest.effective(&clients))
        {
            for c in &clients {
                prop_assert!(small.cols() <= c.size.cols() && small.rows() <= c.size.rows());
                prop_assert!(large.cols() >= c.size.cols() && large.rows() >= c.size.rows());
            }
            prop_assert!(clients.iter().any(|c| c.size.cols() == small.cols()));
            prop_assert!(clients.iter().any(|c| c.size.rows() == large.rows()));
        }
    }

    /// The order clients are listed in never matters.
    #[test]
    fn policies_ignore_client_order(clients in clients(), policy in prop::sample::select(POLICIES.to_vec())) {
        let mut reversed = clients.clone();
        reversed.reverse();
        prop_assert_eq!(policy.effective(&clients), policy.effective(&reversed));
    }

    #[test]
    fn pane_area_reserves_the_status_row(size in grid_size()) {
        let area = pane_area(size);
        prop_assert_eq!((area.x(), area.y(), area.size().cols()), (0, 0, size.cols()));
        let expected_rows = if size.rows().get() == 1 { 1 } else { size.rows().get() - 1 };
        prop_assert_eq!(area.rows(), expected_rows);
    }
}
